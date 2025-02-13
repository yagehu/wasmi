use super::{
    visit_register::VisitInputRegisters,
    FuelInfo,
    LabelRef,
    LabelRegistry,
    TypedProvider,
};
use crate::{
    engine::{
        bytecode::{
            BinInstr,
            BinInstrImm16,
            BranchOffset,
            BranchOffset16,
            Const16,
            Const32,
            Instruction,
            Provider,
            Register,
            RegisterSpan,
            RegisterSpanIter,
        },
        translator::{stack::RegisterSpace, ValueStack},
        FuelCosts,
    },
    module::ModuleHeader,
    Error,
};
use alloc::vec::{Drain, Vec};
use core::mem;
use wasmi_core::{UntypedValue, ValueType, F32};

/// A reference to an instruction of the partially
/// constructed function body of the [`InstrEncoder`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instr(u32);

impl Instr {
    /// Creates an [`Instr`] from the given `usize` value.
    ///
    /// # Note
    ///
    /// This intentionally is an API intended for test purposes only.
    ///
    /// # Panics
    ///
    /// If the `value` exceeds limitations for [`Instr`].
    pub fn from_usize(value: usize) -> Self {
        let value = value.try_into().unwrap_or_else(|error| {
            panic!("invalid index {value} for instruction reference: {error}")
        });
        Self(value)
    }

    /// Returns an `usize` representation of the instruction index.
    pub fn into_usize(self) -> usize {
        self.0 as usize
    }

    /// Creates an [`Instr`] form the given `u32` value.
    pub fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// Returns an `u32` representation of the instruction index.
    pub fn into_u32(self) -> u32 {
        self.0
    }

    /// Returns the absolute distance between `self` and `other`.
    ///
    /// - Returns `0` if `self == other`.
    /// - Returns `1` if `self` is adjacent to `other` in the sequence of instructions.
    /// - etc..
    pub fn distance(self, other: Self) -> u32 {
        self.0.abs_diff(other.0)
    }
}

/// Encodes `wasmi` bytecode instructions to an [`Instruction`] stream.
#[derive(Debug, Default)]
pub struct InstrEncoder {
    /// Already encoded [`Instruction`] words.
    instrs: InstrSequence,
    /// Unresolved and unpinned labels created during function translation.
    labels: LabelRegistry,
    /// The last [`Instruction`] created via [`InstrEncoder::push_instr`].
    last_instr: Option<Instr>,
    /// The first encoded [`Instr`] that is affected by a `local.set` preservation.
    ///
    /// # Note
    ///
    /// This is an optimization to reduce the amount of work performed during
    /// defragmentation of the register space due to `local.set` register
    /// preservations.
    notified_preservation: Option<Instr>,
}

/// The sequence of encoded [`Instruction`].
#[derive(Debug, Default)]
pub struct InstrSequence {
    /// Already encoded [`Instruction`] words.
    instrs: Vec<Instruction>,
}

impl InstrSequence {
    /// Resets the [`InstrSequence`].
    pub fn reset(&mut self) {
        self.instrs.clear();
    }

    /// Returns the next [`Instr`].
    fn next_instr(&self) -> Instr {
        Instr::from_usize(self.instrs.len())
    }

    /// Pushes an [`Instruction`] to the instruction sequence and returns its [`Instr`].
    ///
    /// # Errors
    ///
    /// If there are too many instructions in the instruction sequence.
    fn push(&mut self, instruction: Instruction) -> Result<Instr, Error> {
        let instr = self.next_instr();
        self.instrs.push(instruction);
        Ok(instr)
    }

    /// Pushes an [`Instruction`] before the [`Instruction`] at [`Instr`].
    ///
    /// Returns the [`Instr`] of the [`Instruction`] that was at [`Instr`] before this operation.
    ///
    /// # Note
    ///
    /// - This operation might be costly. Callers are advised to only insert
    ///   instructions near the end of the sequence in order to avoid massive
    ///   copy overhead since all following instructions are required to be
    ///   shifted in memory.
    /// - The `instr` will refer to the inserted [`Instruction`] after this operation.
    ///
    /// # Errors
    ///
    /// If there are too many instructions in the instruction sequence.
    fn push_before(&mut self, instr: Instr, instruction: Instruction) -> Result<Instr, Error> {
        self.instrs.insert(instr.into_usize(), instruction);
        let shifted_instr = instr
            .into_u32()
            .checked_add(1)
            .map(Instr::from_u32)
            .unwrap_or_else(|| panic!("pushed to many instructions to a single function"));
        Ok(shifted_instr)
    }

    /// Returns the [`Instruction`] associated to the [`Instr`] for this [`InstrSequence`].
    ///
    /// # Panics
    ///
    /// If no [`Instruction`] is associated to the [`Instr`] for this [`InstrSequence`].
    fn get(&mut self, instr: Instr) -> &Instruction {
        &self.instrs[instr.into_usize()]
    }

    /// Returns the [`Instruction`] associated to the [`Instr`] for this [`InstrSequence`].
    ///
    /// # Panics
    ///
    /// If no [`Instruction`] is associated to the [`Instr`] for this [`InstrSequence`].
    fn get_mut(&mut self, instr: Instr) -> &mut Instruction {
        &mut self.instrs[instr.into_usize()]
    }

    /// Return an iterator over the sequence of generated [`Instruction`].
    ///
    /// # Note
    ///
    /// The [`InstrSequence`] will be in an empty state after this operation.
    pub fn drain(&mut self) -> Drain<Instruction> {
        self.instrs.drain(..)
    }

    /// Returns a slice to the sequence of [`Instruction`] starting at `start`.
    ///
    /// # Panics
    ///
    /// If `start` is out of bounds for [`InstrSequence`].
    pub fn get_slice_at_mut(&mut self, start: Instr) -> &mut [Instruction] {
        &mut self.instrs[start.into_usize()..]
    }
}

impl<'a> IntoIterator for &'a mut InstrSequence {
    type Item = &'a mut Instruction;
    type IntoIter = core::slice::IterMut<'a, Instruction>;

    fn into_iter(self) -> Self::IntoIter {
        self.instrs.iter_mut()
    }
}

impl InstrEncoder {
    /// Resets the [`InstrEncoder`].
    pub fn reset(&mut self) {
        self.instrs.reset();
        self.labels.reset();
        self.reset_last_instr();
        self.notified_preservation = None;
    }

    /// Resets the [`Instr`] last created via [`InstrEncoder::push_instr`].
    ///
    /// # Note
    ///
    /// The `last_instr` information is used for an optimization with `local.set`
    /// and `local.tee` translation to replace the result [`Register`] of the
    /// last created [`Instruction`] instead of creating another copy [`Instruction`].
    ///
    /// Whenever ending a control block during Wasm translation the `last_instr`
    /// information needs to be reset so that a `local.set` or `local.tee` does
    /// not invalidly optimize across control flow boundaries.
    pub fn reset_last_instr(&mut self) {
        self.last_instr = None;
    }

    /// Return an iterator over the sequence of generated [`Instruction`].
    ///
    /// # Note
    ///
    /// The [`InstrEncoder`] will be in an empty state after this operation.
    pub fn drain_instrs(&mut self) -> Drain<Instruction> {
        self.instrs.drain()
    }

    /// Creates a new unresolved label and returns its [`LabelRef`].
    pub fn new_label(&mut self) -> LabelRef {
        self.labels.new_label()
    }

    /// Resolve the label at the current instruction position.
    ///
    /// Does nothing if the label has already been resolved.
    ///
    /// # Note
    ///
    /// This is used at a position of the Wasm bytecode where it is clear that
    /// the given label can be resolved properly.
    /// This usually takes place when encountering the Wasm `End` operand for example.
    pub fn pin_label_if_unpinned(&mut self, label: LabelRef) {
        self.labels.try_pin_label(label, self.instrs.next_instr())
    }

    /// Resolve the label at the current instruction position.
    ///
    /// # Note
    ///
    /// This is used at a position of the Wasm bytecode where it is clear that
    /// the given label can be resolved properly.
    /// This usually takes place when encountering the Wasm `End` operand for example.
    ///
    /// # Panics
    ///
    /// If the label has already been resolved.
    pub fn pin_label(&mut self, label: LabelRef) {
        self.labels
            .pin_label(label, self.instrs.next_instr())
            .unwrap_or_else(|err| panic!("failed to pin label: {err}"));
    }

    /// Try resolving the [`LabelRef`] for the currently constructed instruction.
    ///
    /// Returns an uninitialized [`BranchOffset`] if the `label` cannot yet
    /// be resolved and defers resolution to later.
    pub fn try_resolve_label(&mut self, label: LabelRef) -> Result<BranchOffset, Error> {
        let user = self.instrs.next_instr();
        self.try_resolve_label_for(label, user)
    }

    /// Try resolving the [`LabelRef`] for the given [`Instr`].
    ///
    /// Returns an uninitialized [`BranchOffset`] if the `label` cannot yet
    /// be resolved and defers resolution to later.
    pub fn try_resolve_label_for(
        &mut self,
        label: LabelRef,
        instr: Instr,
    ) -> Result<BranchOffset, Error> {
        self.labels.try_resolve_label(label, instr)
    }

    /// Updates the branch offsets of all branch instructions inplace.
    ///
    /// # Panics
    ///
    /// If this is used before all branching labels have been pinned.
    pub fn update_branch_offsets(&mut self) -> Result<(), Error> {
        for (user, offset) in self.labels.resolved_users() {
            self.instrs.get_mut(user).update_branch_offset(offset?)?;
        }
        Ok(())
    }

    /// Push the [`Instruction`] to the [`InstrEncoder`].
    pub fn push_instr(&mut self, instr: Instruction) -> Result<Instr, Error> {
        let last_instr = self.instrs.push(instr)?;
        self.last_instr = Some(last_instr);
        Ok(last_instr)
    }

    /// Appends the [`Instruction`] to the last [`Instruction`] created via [`InstrEncoder::push_instr`].
    ///
    /// # Note
    ///
    /// This is used primarily for [`Instruction`] words that are just carrying
    /// parameters for the [`Instruction`]. An example of this is [`Instruction::Const32`]
    /// carrying the `offset` parameter for [`Instruction::I32Load`].
    pub fn append_instr(&mut self, instr: Instruction) -> Result<Instr, Error> {
        self.instrs.push(instr)
    }

    /// Encode a `copy result <- value` instruction.
    ///
    /// # Note
    ///
    /// Applies optimizations for `copy x <- x` and properly selects the
    /// most optimized `copy` instruction variant for the given `value`.
    pub fn encode_copy(
        &mut self,
        stack: &mut ValueStack,
        result: Register,
        value: TypedProvider,
        fuel_info: FuelInfo,
    ) -> Result<Option<Instr>, Error> {
        /// Convenience to create an [`Instruction::Copy`] to copy a constant value.
        fn copy_imm(
            stack: &mut ValueStack,
            result: Register,
            value: impl Into<UntypedValue>,
        ) -> Result<Instruction, Error> {
            let cref = stack.alloc_const(value.into())?;
            Ok(Instruction::copy(result, cref))
        }
        let instr = match value {
            TypedProvider::Register(value) => {
                if result == value {
                    // Optimization: copying from register `x` into `x` is a no-op.
                    return Ok(None);
                }
                Instruction::copy(result, value)
            }
            TypedProvider::Const(value) => match value.ty() {
                ValueType::I32 => Instruction::copy_imm32(result, i32::from(value)),
                ValueType::F32 => Instruction::copy_imm32(result, f32::from(value)),
                ValueType::I64 => match <Const32<i64>>::try_from(i64::from(value)).ok() {
                    Some(value) => Instruction::copy_i64imm32(result, value),
                    None => copy_imm(stack, result, value)?,
                },
                ValueType::F64 => match <Const32<f64>>::try_from(f64::from(value)).ok() {
                    Some(value) => Instruction::copy_f64imm32(result, value),
                    None => copy_imm(stack, result, value)?,
                },
                ValueType::FuncRef => copy_imm(stack, result, value)?,
                ValueType::ExternRef => copy_imm(stack, result, value)?,
            },
        };
        self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
        let instr = self.push_instr(instr)?;
        Ok(Some(instr))
    }

    /// Encode a generic `copy` instruction.
    ///
    /// # Note
    ///
    /// Avoids no-op copies such as `copy x <- x` and properly selects the
    /// most optimized `copy` instruction variant for the given `value`.
    pub fn encode_copies(
        &mut self,
        stack: &mut ValueStack,
        mut results: RegisterSpanIter,
        values: &[TypedProvider],
        fuel_info: FuelInfo,
    ) -> Result<(), Error> {
        assert_eq!(results.len(), values.len());
        if let Some((TypedProvider::Register(value), rest)) = values.split_first() {
            if results.span().head() == *value {
                // Case: `result` and `value` are equal thus this is a no-op copy which we can avoid.
                //       Applied recursively we thereby remove all no-op copies at the start of the
                //       copy sequence until the first actual copy.
                results.next();
                return self.encode_copies(stack, results, rest, fuel_info);
            }
        }
        let result = results.span().head();
        match values {
            [] => {
                // The copy sequence is empty, nothing to encode in this case.
                Ok(())
            }
            [v0] => {
                self.encode_copy(stack, result, *v0, fuel_info)?;
                Ok(())
            }
            [v0, v1] => {
                if TypedProvider::Register(result.next()) == *v1 {
                    // Case: the second of the 2 copies is a no-op which we can avoid
                    // Note: we already asserted that the first copy is not a no-op
                    self.encode_copy(stack, result, *v0, fuel_info)?;
                    return Ok(());
                }
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
                self.push_instr(Instruction::copy2(results.span(), reg0, reg1))?;
                Ok(())
            }
            [v0, v1, rest @ ..] => {
                debug_assert!(!rest.is_empty());
                // Note: The fuel for copies might result in 0 charges if there aren't
                //       enough copies to account for at least 1 fuel. Therefore we need
                //       to also bump by `FuelCosts::base` to charge at least 1 fuel.
                self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
                self.bump_fuel_consumption(fuel_info, |costs| {
                    costs.fuel_for_copies(rest.len() as u64 + 3)
                })?;
                if let Some(values) = RegisterSpanIter::from_providers(values) {
                    let make_instr = match Self::has_overlapping_copy_spans(
                        results.span(),
                        values.span(),
                        values.len(),
                    ) {
                        true => Instruction::copy_span,
                        false => Instruction::copy_span_non_overlapping,
                    };
                    self.push_instr(make_instr(
                        results.span(),
                        values.span(),
                        values.len_as_u16(),
                    ))?;
                    return Ok(());
                }
                let make_instr = match Self::has_overlapping_copies(results, values) {
                    true => Instruction::copy_many,
                    false => Instruction::copy_many_non_overlapping,
                };
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                self.push_instr(make_instr(results.span(), reg0, reg1))?;
                self.encode_register_list(stack, rest)?;
                Ok(())
            }
        }
    }

    /// Returns `true` if `copy_span results <- values` has overlapping copies.
    ///
    /// # Examples
    ///
    /// - `[ ]`: empty never overlaps
    /// - `[ 1 <- 0 ]`: single element never overlaps
    /// - `[ 0 <- 1, 1 <- 2, 2 <- 3 ]``: no overlap
    /// - `[ 1 <- 0, 2 <- 1 ]`: overlaps!
    fn has_overlapping_copy_spans(results: RegisterSpan, values: RegisterSpan, len: usize) -> bool {
        RegisterSpanIter::has_overlapping_copies(results.iter(len), values.iter(len))
    }

    /// Returns `true` if the `copy results <- values` instruction has overlaps.
    ///
    /// # Examples
    ///
    /// - The sequence `[ 0 <- 1, 1 <- 1, 2 <- 4 ]` has no overlapping copies.
    /// - The sequence `[ 0 <- 1, 1 <- 0 ]` has overlapping copies since register `0`
    ///   is written to in the first copy but read from in the next.
    /// - The sequence `[ 3 <- 1, 4 <- 2, 5 <- 3 ]` has overlapping copies since register `3`
    ///   is written to in the first copy but read from in the third.
    fn has_overlapping_copies(results: RegisterSpanIter, values: &[TypedProvider]) -> bool {
        debug_assert_eq!(results.len(), values.len());
        if results.is_empty() {
            // Note: An empty set of copies can never have overlapping copies.
            return false;
        }
        let result0 = results.span().head();
        for (result, value) in results.zip(values) {
            // Note: We only have to check the register case since constant value
            //       copies can never overlap.
            if let TypedProvider::Register(value) = *value {
                // If the register `value` index is within range of `result0..result`
                // then its value has been overwritten by previous copies.
                if result0 <= value && value < result {
                    return true;
                }
            }
        }
        false
    }

    /// Bumps consumed fuel for [`Instruction::ConsumeFuel`] of `instr` by `delta`.
    ///
    /// # Errors
    ///
    /// If consumed fuel is out of bounds after this operation.
    pub fn bump_fuel_consumption<F>(&mut self, fuel_info: FuelInfo, f: F) -> Result<(), Error>
    where
        F: FnOnce(&FuelCosts) -> u64,
    {
        let FuelInfo::Some { costs, instr } = fuel_info else {
            // Fuel metering is disabled so we can bail out.
            return Ok(());
        };
        let fuel_consumed = f(&costs);
        self.instrs
            .get_mut(instr)
            .bump_fuel_consumption(fuel_consumed)?;
        Ok(())
    }

    /// Encodes an unconditional `return` instruction.
    pub fn encode_return(
        &mut self,
        stack: &mut ValueStack,
        values: &[TypedProvider],
        fuel_info: FuelInfo,
    ) -> Result<(), Error> {
        let instr = match values {
            [] => Instruction::Return,
            [TypedProvider::Register(reg)] => Instruction::return_reg(*reg),
            [TypedProvider::Const(value)] => match value.ty() {
                ValueType::I32 => Instruction::return_imm32(i32::from(*value)),
                ValueType::I64 => match <Const32<i64>>::try_from(i64::from(*value)).ok() {
                    Some(value) => Instruction::return_i64imm32(value),
                    None => Instruction::return_reg(stack.alloc_const(*value)?),
                },
                ValueType::F32 => Instruction::return_imm32(F32::from(*value)),
                ValueType::F64 => match <Const32<f64>>::try_from(f64::from(*value)).ok() {
                    Some(value) => Instruction::return_f64imm32(value),
                    None => Instruction::return_reg(stack.alloc_const(*value)?),
                },
                ValueType::FuncRef | ValueType::ExternRef => {
                    Instruction::return_reg(stack.alloc_const(*value)?)
                }
            },
            [v0, v1] => {
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                Instruction::return_reg2(reg0, reg1)
            }
            [v0, v1, v2] => {
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                let reg2 = Self::provider2reg(stack, v2)?;
                Instruction::return_reg3(reg0, reg1, reg2)
            }
            [v0, v1, v2, rest @ ..] => {
                debug_assert!(!rest.is_empty());
                // Note: The fuel for return values might result in 0 charges if there aren't
                //       enough return values to account for at least 1 fuel. Therefore we need
                //       to also bump by `FuelCosts::base` to charge at least 1 fuel.
                self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
                self.bump_fuel_consumption(fuel_info, |costs| {
                    costs.fuel_for_copies(rest.len() as u64 + 3)
                })?;
                if let Some(span) = RegisterSpanIter::from_providers(values) {
                    self.push_instr(Instruction::return_span(span))?;
                    return Ok(());
                }
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                let reg2 = Self::provider2reg(stack, v2)?;
                self.push_instr(Instruction::return_many(reg0, reg1, reg2))?;
                self.encode_register_list(stack, rest)?;
                return Ok(());
            }
        };
        self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
        self.push_instr(instr)?;
        Ok(())
    }

    /// Encodes an conditional `return` instruction.
    pub fn encode_return_nez(
        &mut self,
        stack: &mut ValueStack,
        condition: Register,
        values: &[TypedProvider],
        fuel_info: FuelInfo,
    ) -> Result<(), Error> {
        // Note: We bump fuel unconditionally even if the conditional return is not taken.
        //       This is very conservative and may lead to more fuel costs than
        //       actually needed for the computation. We might revisit this decision
        //       later. An alternative solution would consume fuel during execution
        //       time only when the return is taken.
        let instr = match values {
            [] => Instruction::return_nez(condition),
            [TypedProvider::Register(reg)] => Instruction::return_nez_reg(condition, *reg),
            [TypedProvider::Const(value)] => match value.ty() {
                ValueType::I32 => Instruction::return_nez_imm32(condition, i32::from(*value)),
                ValueType::I64 => match <Const32<i64>>::try_from(i64::from(*value)).ok() {
                    Some(value) => Instruction::return_nez_i64imm32(condition, value),
                    None => Instruction::return_nez_reg(condition, stack.alloc_const(*value)?),
                },
                ValueType::F32 => Instruction::return_nez_imm32(condition, F32::from(*value)),
                ValueType::F64 => match <Const32<f64>>::try_from(f64::from(*value)).ok() {
                    Some(value) => Instruction::return_nez_f64imm32(condition, value),
                    None => Instruction::return_nez_reg(condition, stack.alloc_const(*value)?),
                },
                ValueType::FuncRef | ValueType::ExternRef => {
                    Instruction::return_nez_reg(condition, stack.alloc_const(*value)?)
                }
            },
            [v0, v1] => {
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                Instruction::return_nez_reg2(condition, reg0, reg1)
            }
            [v0, v1, rest @ ..] => {
                debug_assert!(!rest.is_empty());
                // Note: The fuel for return values might result in 0 charges if there aren't
                //       enough return values to account for at least 1 fuel. Therefore we need
                //       to also bump by `FuelCosts::base` to charge at least 1 fuel.
                self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
                self.bump_fuel_consumption(fuel_info, |costs| {
                    costs.fuel_for_copies(rest.len() as u64 + 3)
                })?;
                if let Some(span) = RegisterSpanIter::from_providers(values) {
                    self.push_instr(Instruction::return_nez_span(condition, span))?;
                    return Ok(());
                }
                let reg0 = Self::provider2reg(stack, v0)?;
                let reg1 = Self::provider2reg(stack, v1)?;
                self.push_instr(Instruction::return_nez_many(condition, reg0, reg1))?;
                self.encode_register_list(stack, rest)?;
                return Ok(());
            }
        };
        self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
        self.push_instr(instr)?;
        Ok(())
    }

    /// Converts a [`TypedProvider`] into a [`Register`].
    ///
    /// This allocates constant values for [`TypedProvider::Const`].
    fn provider2reg(stack: &mut ValueStack, provider: &TypedProvider) -> Result<Register, Error> {
        match provider {
            Provider::Register(register) => Ok(*register),
            Provider::Const(value) => stack.alloc_const(*value),
        }
    }

    /// Encode the given slice of [`TypedProvider`] as a list of [`Register`].
    ///
    /// # Note
    ///
    /// This is used for the following n-ary instructions:
    ///
    /// - [`Instruction::ReturnMany`]
    /// - [`Instruction::ReturnNezMany`]
    /// - [`Instruction::CopyMany`]
    /// - [`Instruction::CallInternal`]
    /// - [`Instruction::CallImported`]
    /// - [`Instruction::CallIndirect`]
    /// - [`Instruction::ReturnCallInternal`]
    /// - [`Instruction::ReturnCallImported`]
    /// - [`Instruction::ReturnCallIndirect`]
    pub fn encode_register_list(
        &mut self,
        stack: &mut ValueStack,
        inputs: &[TypedProvider],
    ) -> Result<(), Error> {
        let mut remaining = inputs;
        loop {
            match remaining {
                [] => return Ok(()),
                [v0] => {
                    let v0 = Self::provider2reg(stack, v0)?;
                    self.instrs.push(Instruction::register(v0))?;
                    return Ok(());
                }
                [v0, v1] => {
                    let v0 = Self::provider2reg(stack, v0)?;
                    let v1 = Self::provider2reg(stack, v1)?;
                    self.instrs.push(Instruction::register2(v0, v1))?;
                    return Ok(());
                }
                [v0, v1, v2] => {
                    let v0 = Self::provider2reg(stack, v0)?;
                    let v1 = Self::provider2reg(stack, v1)?;
                    let v2 = Self::provider2reg(stack, v2)?;
                    self.instrs.push(Instruction::register3(v0, v1, v2))?;
                    return Ok(());
                }
                [v0, v1, v2, rest @ ..] => {
                    let v0 = Self::provider2reg(stack, v0)?;
                    let v1 = Self::provider2reg(stack, v1)?;
                    let v2 = Self::provider2reg(stack, v2)?;
                    self.instrs.push(Instruction::register_list(v0, v1, v2))?;
                    remaining = rest;
                }
            }
        }
    }

    /// Encode a `local.set` or `local.tee` instruction.
    ///
    /// This also applies an optimization in that the previous instruction
    /// result is replaced with the `local` [`Register`] instead of encoding
    /// another `copy` instruction if the `local.set` or `local.tee` belongs
    /// to the same basic block.
    ///
    /// # Note
    ///
    /// - If `value` is a [`Register`] it usually is equal to the
    ///   result [`Register`] of the previous instruction.
    pub fn encode_local_set(
        &mut self,
        stack: &mut ValueStack,
        res: &ModuleHeader,
        local: Register,
        value: TypedProvider,
        preserved: Option<Register>,
        fuel_info: FuelInfo,
    ) -> Result<(), Error> {
        fn fallback_case(
            this: &mut InstrEncoder,
            stack: &mut ValueStack,
            local: Register,
            value: TypedProvider,
            preserved: Option<Register>,
            fuel_info: FuelInfo,
        ) -> Result<(), Error> {
            if let Some(preserved) = preserved {
                this.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
                let preserve_instr = this.push_instr(Instruction::copy(preserved, local))?;
                this.notify_preserved_register(preserve_instr);
            }
            this.encode_copy(stack, local, value, fuel_info)?;
            Ok(())
        }

        debug_assert!(matches!(
            stack.get_register_space(local),
            RegisterSpace::Local
        ));
        let TypedProvider::Register(returned_value) = value else {
            // Cannot apply the optimization for `local.set C` where `C` is a constant value.
            return fallback_case(self, stack, local, value, preserved, fuel_info);
        };
        if matches!(
            stack.get_register_space(returned_value),
            RegisterSpace::Local
        ) {
            // Can only apply the optimization if the returned value of `last_instr`
            // is _NOT_ itself a local register due to observable behavior.
            return fallback_case(self, stack, local, value, preserved, fuel_info);
        }
        let Some(last_instr) = self.last_instr else {
            // Can only apply the optimization if there is a previous instruction
            // to replace its result register instead of emitting a copy.
            return fallback_case(self, stack, local, value, preserved, fuel_info);
        };
        if preserved.is_some() && last_instr.distance(self.instrs.next_instr()) >= 4 {
            // We avoid applying the optimization if the last instruction
            // has a very large encoding, e.g. for function calls with lots
            // of parameters. This is because the optimization while also
            // preserving a local register requires costly shifting all
            // instruction words of the last instruction.
            // Thankfully most instructions are small enough.
            return fallback_case(self, stack, local, value, preserved, fuel_info);
        }
        if !self
            .instrs
            .get_mut(last_instr)
            .relink_result(res, local, returned_value)?
        {
            // It was not possible to relink the result of `last_instr` therefore we fallback.
            return fallback_case(self, stack, local, value, preserved, fuel_info);
        }
        if let Some(preserved) = preserved {
            // We were able to apply the optimization.
            // Preservation requires the copy to be before the optimized last instruction.
            // Therefore we need to push the preservation `copy` instruction before it.
            self.bump_fuel_consumption(fuel_info, FuelCosts::base)?;
            let shifted_last_instr = self
                .instrs
                .push_before(last_instr, Instruction::copy(preserved, local))?;
            self.notify_preserved_register(last_instr);
            self.last_instr = Some(shifted_last_instr);
        }
        Ok(())
    }

    /// Notifies the [`InstrEncoder`] that a local variable has been preserved.
    ///
    /// # Note
    ///
    /// This is an optimization that we perform to avoid or minimize the work
    /// done in [`InstrEncoder::defrag_registers`] by either avoiding defragmentation
    /// entirely if no local preservations took place or by at least only defragmenting
    /// the slice of instructions that could have been affected by it but not all
    /// encoded instructions.
    /// Only instructions that are encoded after the preservation could have been affected.
    ///
    /// This will ignore any preservation notifications after the first one.
    pub fn notify_preserved_register(&mut self, preserve_instr: Instr) {
        debug_assert!(
            matches!(self.instrs.get(preserve_instr), Instruction::Copy { .. }),
            "a preserve instruction is always a register copy instruction"
        );
        if self.notified_preservation.is_none() {
            self.notified_preservation = Some(preserve_instr);
        }
    }

    /// Defragments storage-space registers of all encoded [`Instruction`].
    pub fn defrag_registers(&mut self, stack: &mut ValueStack) -> Result<(), Error> {
        stack.finalize_alloc();
        if let Some(notified_preserved) = self.notified_preservation {
            for instr in self.instrs.get_slice_at_mut(notified_preserved) {
                instr.visit_input_registers(|reg| *reg = stack.defrag_register(*reg));
            }
        }
        Ok(())
    }

    /// Translates a Wasm `i32.eqz` instruction.
    ///
    /// Tries to fuse `i32.eqz` with a previous `i32.{and,or,xor}` instruction if possible.
    /// Returns `true` if it was possible to fuse the `i32.eqz` instruction.
    pub fn fuse_i32_eqz(&mut self, stack: &mut ValueStack) -> bool {
        /// Fuse a `i32.{and,or,xor}` instruction with `i32.eqz`.
        macro_rules! fuse {
            ($instr:ident, $stack:ident, $make_fuse:expr) => {{
                if matches!(
                    $stack.get_register_space($instr.result),
                    RegisterSpace::Local
                ) {
                    return false;
                }
                $make_fuse($instr.result, $instr.lhs, $instr.rhs)
            }};
        }

        /// Fuse a `i32.{and,or,xor}` instruction with 16-bit encoded immediate parameter with `i32.eqz`.
        macro_rules! fuse_imm16 {
            ($instr:ident, $stack:ident, $make_fuse:expr) => {{
                if matches!(
                    $stack.get_register_space($instr.result),
                    RegisterSpace::Local
                ) {
                    // Must not fuse instruction that store to local registers since
                    // this behavior is observable and would not be semantics preserving.
                    return false;
                }
                $make_fuse($instr.result, $instr.reg_in, $instr.imm_in)
            }};
        }

        let Some(last_instr) = self.last_instr else {
            return false;
        };
        let fused_instr = match self.instrs.get(last_instr) {
            Instruction::I32And(instr) => fuse!(instr, stack, Instruction::i32_and_eqz),
            Instruction::I32AndImm16(instr) => {
                fuse_imm16!(instr, stack, Instruction::i32_and_eqz_imm16)
            }
            Instruction::I32Or(instr) => fuse!(instr, stack, Instruction::i32_or_eqz),
            Instruction::I32OrImm16(instr) => {
                fuse_imm16!(instr, stack, Instruction::i32_or_eqz_imm16)
            }
            Instruction::I32Xor(instr) => fuse!(instr, stack, Instruction::i32_xor_eqz),
            Instruction::I32XorImm16(instr) => {
                fuse_imm16!(instr, stack, Instruction::i32_xor_eqz_imm16)
            }
            _ => return false,
        };
        _ = mem::replace(self.instrs.get_mut(last_instr), fused_instr);
        true
    }

    /// Encodes a `branch_eqz` instruction and tries to fuse it with a previous comparison instruction.
    pub fn encode_branch_eqz(
        &mut self,
        stack: &mut ValueStack,
        condition: Register,
        label: LabelRef,
    ) -> Result<(), Error> {
        type BranchCmpConstructor = fn(Register, Register, BranchOffset16) -> Instruction;
        type BranchCmpImmConstructor<T> = fn(Register, Const16<T>, BranchOffset16) -> Instruction;

        /// Encode an unoptimized `branch_eqz` instruction.
        ///
        /// This is used as fallback whenever fusing compare and branch instructions is not possible.
        fn encode_branch_eqz_fallback(
            this: &mut InstrEncoder,
            condition: Register,
            label: LabelRef,
        ) -> Result<(), Error> {
            let offset = this
                .try_resolve_label(label)
                .and_then(BranchOffset16::try_from)?;
            this.push_instr(Instruction::branch_i32_eqz(condition, offset))?;
            Ok(())
        }

        /// Create a fused cmp+branch instruction and wrap it in a `Some`.
        ///
        /// We wrap the returned value in `Some` to unify handling of a bunch of cases.
        fn fuse(
            this: &mut InstrEncoder,
            stack: &mut ValueStack,
            last_instr: Instr,
            instr: BinInstr,
            label: LabelRef,
            make_instr: BranchCmpConstructor,
        ) -> Result<Option<Instruction>, Error> {
            if matches!(stack.get_register_space(instr.result), RegisterSpace::Local) {
                // We need to filter out instructions that store their result
                // into a local register slot because they introduce observable behavior
                // which a fused cmp+branch instruction would remove.
                return Ok(None);
            }
            let offset = this.try_resolve_label_for(label, last_instr)?;
            let instr = BranchOffset16::new(offset)
                .map(|offset16| make_instr(instr.lhs, instr.rhs, offset16));
            Ok(instr)
        }

        /// Create a fused cmp+branch instruction with a 16-bit immediate and wrap it in a `Some`.
        ///
        /// We wrap the returned value in `Some` to unify handling of a bunch of cases.
        fn fuse_imm<T>(
            this: &mut InstrEncoder,
            stack: &mut ValueStack,
            last_instr: Instr,
            instr: BinInstrImm16<T>,
            label: LabelRef,
            make_instr: BranchCmpImmConstructor<T>,
        ) -> Result<Option<Instruction>, Error> {
            if matches!(stack.get_register_space(instr.result), RegisterSpace::Local) {
                // We need to filter out instructions that store their result
                // into a local register slot because they introduce observable behavior
                // which a fused cmp+branch instruction would remove.
                return Ok(None);
            }
            let offset = this.try_resolve_label_for(label, last_instr)?;
            let instr = BranchOffset16::new(offset)
                .map(|offset16| make_instr(instr.reg_in, instr.imm_in, offset16));
            Ok(instr)
        }
        use Instruction as I;

        let Some(last_instr) = self.last_instr else {
            return encode_branch_eqz_fallback(self, condition, label);
        };

        #[rustfmt::skip]
        let fused_instr = match *self.instrs.get(last_instr) {
            I::I32EqImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i32_nez(instr.reg_in, offset16))
                    }
                }
            }
            I::I64EqImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i64_nez(instr.reg_in, offset16))
                    }
                }
            }
            I::I32NeImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i32_eqz(instr.reg_in, offset16))
                    }
                }
            }
            I::I64NeImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i64_eqz(instr.reg_in, offset16))
                    }
                }
            }
            I::I32And(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_and_eqz as _)?,
            I::I32Or(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_or_eqz as _)?,
            I::I32Xor(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_xor_eqz as _)?,
            I::I32AndEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_and as _)?,
            I::I32OrEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_or as _)?,
            I::I32XorEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_xor as _)?,
            I::I32Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ne as _)?,
            I::I32Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_eq as _)?,
            I::I32LtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ge_s as _)?,
            I::I32LtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ge_u as _)?,
            I::I32LeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_gt_s as _)?,
            I::I32LeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_gt_u as _)?,
            I::I32GtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_le_s as _)?,
            I::I32GtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_le_u as _)?,
            I::I32GeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_lt_s as _)?,
            I::I32GeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_lt_u as _)?,
            I::I64Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ne as _)?,
            I::I64Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_eq as _)?,
            I::I64LtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ge_s as _)?,
            I::I64LtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ge_u as _)?,
            I::I64LeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_gt_s as _)?,
            I::I64LeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_gt_u as _)?,
            I::I64GtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_le_s as _)?,
            I::I64GtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_le_u as _)?,
            I::I64GeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_lt_s as _)?,
            I::I64GeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_lt_u as _)?,
            I::F32Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_ne as _)?,
            I::F32Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_eq as _)?,
            // Note: We cannot fuse cmp+branch for float comparison operators due to how NaN values are treated.
            I::I32AndImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_and_eqz_imm as _)?,
            I::I32OrImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_or_eqz_imm as _)?,
            I::I32XorImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_xor_eqz_imm as _)?,
            I::I32AndEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_and_imm as _)?,
            I::I32OrEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_or_imm as _)?,
            I::I32XorEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_xor_imm as _)?,
            I::I32EqImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ne_imm as _)?,
            I::I32NeImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_eq_imm as _)?,
            I::I32LtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ge_s_imm as _)?,
            I::I32LtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ge_u_imm as _)?,
            I::I32LeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_gt_s_imm as _)?,
            I::I32LeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_gt_u_imm as _)?,
            I::I32GtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_le_s_imm as _)?,
            I::I32GtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_le_u_imm as _)?,
            I::I32GeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_lt_s_imm as _)?,
            I::I32GeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_lt_u_imm as _)?,
            I::I64EqImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ne_imm as _)?,
            I::I64NeImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_eq_imm as _)?,
            I::I64LtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ge_s_imm as _)?,
            I::I64LtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ge_u_imm as _)?,
            I::I64LeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_gt_s_imm as _)?,
            I::I64LeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_gt_u_imm as _)?,
            I::I64GtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_le_s_imm as _)?,
            I::I64GtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_le_u_imm as _)?,
            I::I64GeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_lt_s_imm as _)?,
            I::I64GeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_lt_u_imm as _)?,
            _ => None,
        };
        if let Some(fused_instr) = fused_instr {
            _ = mem::replace(self.instrs.get_mut(last_instr), fused_instr);
            return Ok(());
        }
        encode_branch_eqz_fallback(self, condition, label)
    }

    /// Encodes a `branch_nez` instruction and tries to fuse it with a previous comparison instruction.
    pub fn encode_branch_nez(
        &mut self,
        stack: &mut ValueStack,
        condition: Register,
        label: LabelRef,
    ) -> Result<(), Error> {
        type BranchCmpConstructor = fn(Register, Register, BranchOffset16) -> Instruction;
        type BranchCmpImmConstructor<T> = fn(Register, Const16<T>, BranchOffset16) -> Instruction;

        /// Encode an unoptimized `branch_nez` instruction.
        ///
        /// This is used as fallback whenever fusing compare and branch instructions is not possible.
        fn encode_branch_nez_fallback(
            this: &mut InstrEncoder,
            condition: Register,
            label: LabelRef,
        ) -> Result<(), Error> {
            let offset = this
                .try_resolve_label(label)
                .and_then(BranchOffset16::try_from)?;
            this.push_instr(Instruction::branch_i32_nez(condition, offset))?;
            Ok(())
        }

        /// Create a fused cmp+branch instruction and wrap it in a `Some`.
        ///
        /// We wrap the returned value in `Some` to unify handling of a bunch of cases.
        fn fuse(
            this: &mut InstrEncoder,
            stack: &mut ValueStack,
            last_instr: Instr,
            instr: BinInstr,
            label: LabelRef,
            make_instr: BranchCmpConstructor,
        ) -> Result<Option<Instruction>, Error> {
            if matches!(stack.get_register_space(instr.result), RegisterSpace::Local) {
                // We need to filter out instructions that store their result
                // into a local register slot because they introduce observable behavior
                // which a fused cmp+branch instruction would remove.
                return Ok(None);
            }
            let offset = this.try_resolve_label_for(label, last_instr)?;
            let instr = BranchOffset16::new(offset)
                .map(|offset16| make_instr(instr.lhs, instr.rhs, offset16));
            Ok(instr)
        }

        /// Create a fused cmp+branch instruction with a 16-bit immediate and wrap it in a `Some`.
        ///
        /// We wrap the returned value in `Some` to unify handling of a bunch of cases.
        fn fuse_imm<T>(
            this: &mut InstrEncoder,
            stack: &mut ValueStack,
            last_instr: Instr,
            instr: BinInstrImm16<T>,
            label: LabelRef,
            make_instr: BranchCmpImmConstructor<T>,
        ) -> Result<Option<Instruction>, Error> {
            if matches!(stack.get_register_space(instr.result), RegisterSpace::Local) {
                // We need to filter out instructions that store their result
                // into a local register slot because they introduce observable behavior
                // which a fused cmp+branch instruction would remove.
                return Ok(None);
            }
            let offset = this.try_resolve_label_for(label, last_instr)?;
            let instr = BranchOffset16::new(offset)
                .map(|offset16| make_instr(instr.reg_in, instr.imm_in, offset16));
            Ok(instr)
        }
        use Instruction as I;

        let Some(last_instr) = self.last_instr else {
            return encode_branch_nez_fallback(self, condition, label);
        };

        #[rustfmt::skip]
        let fused_instr = match *self.instrs.get(last_instr) {
            I::I32EqImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i32_eqz(instr.reg_in, offset16))
                    }
                }
            }
            I::I64EqImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i64_eqz(instr.reg_in, offset16))
                    }
                }
            }
            I::I32NeImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i32_nez(instr.reg_in, offset16))
                    }
                }
            }
            I::I64NeImm16(instr) if instr.imm_in.is_zero() => {
                match stack.get_register_space(instr.result) {
                    RegisterSpace::Local => None,
                    _ => {
                        let offset16 = self.try_resolve_label_for(label, last_instr)
                            .and_then(BranchOffset16::try_from)?;
                        Some(Instruction::branch_i64_nez(instr.reg_in, offset16))
                    }
                }
            }
            I::I32And(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_and as _)?,
            I::I32Or(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_or as _)?,
            I::I32Xor(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_xor as _)?,
            I::I32AndEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_and_eqz as _)?,
            I::I32OrEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_or_eqz as _)?,
            I::I32XorEqz(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_xor_eqz as _)?,
            I::I32Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_eq as _)?,
            I::I32Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ne as _)?,
            I::I32LtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_lt_s as _)?,
            I::I32LtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_lt_u as _)?,
            I::I32LeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_le_s as _)?,
            I::I32LeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_le_u as _)?,
            I::I32GtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_gt_s as _)?,
            I::I32GtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_gt_u as _)?,
            I::I32GeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ge_s as _)?,
            I::I32GeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i32_ge_u as _)?,
            I::I64Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_eq as _)?,
            I::I64Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ne as _)?,
            I::I64LtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_lt_s as _)?,
            I::I64LtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_lt_u as _)?,
            I::I64LeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_le_s as _)?,
            I::I64LeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_le_u as _)?,
            I::I64GtS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_gt_s as _)?,
            I::I64GtU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_gt_u as _)?,
            I::I64GeS(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ge_s as _)?,
            I::I64GeU(instr) => fuse(self, stack, last_instr, instr, label, I::branch_i64_ge_u as _)?,
            I::F32Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_eq as _)?,
            I::F32Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_ne as _)?,
            I::F32Lt(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_lt as _)?,
            I::F32Le(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_le as _)?,
            I::F32Gt(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_gt as _)?,
            I::F32Ge(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f32_ge as _)?,
            I::F64Eq(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_eq as _)?,
            I::F64Ne(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_ne as _)?,
            I::F64Lt(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_lt as _)?,
            I::F64Le(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_le as _)?,
            I::F64Gt(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_gt as _)?,
            I::F64Ge(instr) => fuse(self, stack, last_instr, instr, label, I::branch_f64_ge as _)?,
            I::I32AndImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_and_imm as _)?,
            I::I32OrImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_or_imm as _)?,
            I::I32XorImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_xor_imm as _)?,
            I::I32AndEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_and_eqz_imm as _)?,
            I::I32OrEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_or_eqz_imm as _)?,
            I::I32XorEqzImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_xor_eqz_imm as _)?,
            I::I32EqImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_eq_imm as _)?,
            I::I32NeImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ne_imm as _)?,
            I::I32LtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_lt_s_imm as _)?,
            I::I32LtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_lt_u_imm as _)?,
            I::I32LeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_le_s_imm as _)?,
            I::I32LeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_le_u_imm as _)?,
            I::I32GtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_gt_s_imm as _)?,
            I::I32GtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_gt_u_imm as _)?,
            I::I32GeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ge_s_imm as _)?,
            I::I32GeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i32_ge_u_imm as _)?,
            I::I64EqImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_eq_imm as _)?,
            I::I64NeImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ne_imm as _)?,
            I::I64LtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_lt_s_imm as _)?,
            I::I64LtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_lt_u_imm as _)?,
            I::I64LeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_le_s_imm as _)?,
            I::I64LeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_le_u_imm as _)?,
            I::I64GtSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_gt_s_imm as _)?,
            I::I64GtUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_gt_u_imm as _)?,
            I::I64GeSImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ge_s_imm as _)?,
            I::I64GeUImm16(instr) => fuse_imm(self, stack, last_instr, instr, label, I::branch_i64_ge_u_imm as _)?,
            _ => None,
        };
        if let Some(fused_instr) = fused_instr {
            _ = mem::replace(self.instrs.get_mut(last_instr), fused_instr);
            return Ok(());
        }
        encode_branch_nez_fallback(self, condition, label)
    }
}

impl Instruction {
    /// Updates the [`BranchOffset`] for the branch [`Instruction].
    ///
    /// # Panics
    ///
    /// If `self` is not a branch [`Instruction`].
    pub fn update_branch_offset(&mut self, new_offset: BranchOffset) -> Result<(), Error> {
        match self {
            Instruction::Branch { offset } => {
                offset.init(new_offset);
                Ok(())
            }
            Instruction::BranchI32And(instr)
            | Instruction::BranchI32Or(instr)
            | Instruction::BranchI32Xor(instr)
            | Instruction::BranchI32AndEqz(instr)
            | Instruction::BranchI32OrEqz(instr)
            | Instruction::BranchI32XorEqz(instr)
            | Instruction::BranchI32Eq(instr)
            | Instruction::BranchI32Ne(instr)
            | Instruction::BranchI32LtS(instr)
            | Instruction::BranchI32LtU(instr)
            | Instruction::BranchI32LeS(instr)
            | Instruction::BranchI32LeU(instr)
            | Instruction::BranchI32GtS(instr)
            | Instruction::BranchI32GtU(instr)
            | Instruction::BranchI32GeS(instr)
            | Instruction::BranchI32GeU(instr)
            | Instruction::BranchI64Eq(instr)
            | Instruction::BranchI64Ne(instr)
            | Instruction::BranchI64LtS(instr)
            | Instruction::BranchI64LtU(instr)
            | Instruction::BranchI64LeS(instr)
            | Instruction::BranchI64LeU(instr)
            | Instruction::BranchI64GtS(instr)
            | Instruction::BranchI64GtU(instr)
            | Instruction::BranchI64GeS(instr)
            | Instruction::BranchI64GeU(instr)
            | Instruction::BranchF32Eq(instr)
            | Instruction::BranchF32Ne(instr)
            | Instruction::BranchF32Lt(instr)
            | Instruction::BranchF32Le(instr)
            | Instruction::BranchF32Gt(instr)
            | Instruction::BranchF32Ge(instr)
            | Instruction::BranchF64Eq(instr)
            | Instruction::BranchF64Ne(instr)
            | Instruction::BranchF64Lt(instr)
            | Instruction::BranchF64Le(instr)
            | Instruction::BranchF64Gt(instr)
            | Instruction::BranchF64Ge(instr) => instr.offset.init(new_offset),
            Instruction::BranchI32AndImm(instr)
            | Instruction::BranchI32OrImm(instr)
            | Instruction::BranchI32XorImm(instr)
            | Instruction::BranchI32AndEqzImm(instr)
            | Instruction::BranchI32OrEqzImm(instr)
            | Instruction::BranchI32XorEqzImm(instr)
            | Instruction::BranchI32EqImm(instr)
            | Instruction::BranchI32NeImm(instr)
            | Instruction::BranchI32LtSImm(instr)
            | Instruction::BranchI32LeSImm(instr)
            | Instruction::BranchI32GtSImm(instr)
            | Instruction::BranchI32GeSImm(instr) => instr.offset.init(new_offset),
            Instruction::BranchI32LtUImm(instr)
            | Instruction::BranchI32LeUImm(instr)
            | Instruction::BranchI32GtUImm(instr)
            | Instruction::BranchI32GeUImm(instr) => instr.offset.init(new_offset),
            Instruction::BranchI64EqImm(instr)
            | Instruction::BranchI64NeImm(instr)
            | Instruction::BranchI64LtSImm(instr)
            | Instruction::BranchI64LeSImm(instr)
            | Instruction::BranchI64GtSImm(instr)
            | Instruction::BranchI64GeSImm(instr) => instr.offset.init(new_offset),
            Instruction::BranchI64LtUImm(instr)
            | Instruction::BranchI64LeUImm(instr)
            | Instruction::BranchI64GtUImm(instr)
            | Instruction::BranchI64GeUImm(instr) => instr.offset.init(new_offset),
            _ => panic!("tried to update branch offset of a non-branch instruction: {self:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{bytecode::RegisterSpan, translator::typed_value::TypedValue};

    #[test]
    fn has_overlapping_copies_works() {
        assert!(!InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(0)).iter(0),
            &[],
        ));
        assert!(!InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(0)).iter(2),
            &[TypedProvider::register(0), TypedProvider::register(1),],
        ));
        assert!(!InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(0)).iter(2),
            &[
                TypedProvider::Const(TypedValue::from(10_i32)),
                TypedProvider::Const(TypedValue::from(20_i32)),
            ],
        ));
        assert!(InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(0)).iter(2),
            &[
                TypedProvider::Const(TypedValue::from(10_i32)),
                TypedProvider::register(0),
            ],
        ));
        assert!(InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(0)).iter(2),
            &[TypedProvider::register(0), TypedProvider::register(0),],
        ));
        assert!(InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(3)).iter(3),
            &[
                TypedProvider::register(2),
                TypedProvider::register(3),
                TypedProvider::register(2),
            ],
        ));
        assert!(InstrEncoder::has_overlapping_copies(
            RegisterSpan::new(Register::from_i16(3)).iter(4),
            &[
                TypedProvider::register(-1),
                TypedProvider::register(10),
                TypedProvider::register(2),
                TypedProvider::register(4),
            ],
        ));
    }
}
