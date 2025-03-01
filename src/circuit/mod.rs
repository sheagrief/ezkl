///
pub mod table;

///
pub mod utils;

///
pub mod ops;
pub use ops::*;

/// Tests
#[cfg(test)]
mod tests;

use thiserror::Error;

use halo2_proofs::{
    arithmetic::Field,
    circuit::{Layouter, Region},
    plonk::{ConstraintSystem, Constraints, Expression, Selector},
    poly::Rotation,
};
use halo2curves::FieldExt;
use log::warn;
use serde::{Deserialize, Serialize};

use crate::{
    circuit::ops::base::BaseOp,
    fieldutils::i32_to_felt,
    tensor::{Tensor, TensorType, ValTensor, VarTensor},
};
use std::{collections::BTreeMap, error::Error, marker::PhantomData};

use self::{ops::lookup::LookupOp, table::Table};

/// circuit related errors.
#[derive(Debug, Error)]
pub enum CircuitError {
    /// Shape mismatch in circuit construction
    #[error("dimension mismatch in circuit construction for op: {0}")]
    DimMismatch(String),
    /// Error when instantiating lookup tables
    #[error("failed to instantiate lookup tables")]
    LookupInstantiation,
    /// A lookup table was was already assigned
    #[error("attempting to initialize an already instantiated lookup table")]
    TableAlreadyAssigned,
    /// This operation is unsupported
    #[error("unsupported operation in graph")]
    UnsupportedOp,
}

#[allow(missing_docs)]
/// An enum representing activating the sanity checks we can perform on the accumulated arguments
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default, Copy,
)]
pub enum CheckMode {
    #[default]
    SAFE,
    UNSAFE,
}

impl From<String> for CheckMode {
    fn from(value: String) -> Self {
        match value.to_lowercase().as_str() {
            "safe" => CheckMode::SAFE,
            "unsafe" => CheckMode::UNSAFE,
            _ => panic!("not a valid checkmode"),
        }
    }
}

/// Configuration for an accumulated arg.
#[derive(Clone, Debug, Default)]
pub struct BaseConfig<F: FieldExt + TensorType> {
    /// the inputs to the accumulated operations.
    pub inputs: Vec<VarTensor>,
    /// the VarTensor reserved for lookup operations (could be an element of inputs)
    /// Note that you should be careful to ensure that the lookup_input is not simultaneously assigned to by other non-lookup operations eg. in the case of composite ops.
    pub lookup_input: VarTensor,
    /// the (currently singular) output of the accumulated operations.
    pub output: VarTensor,
    ///  the VarTensor reserved for lookup operations (could be an element of inputs or the same as output)
    /// Note that you should be careful to ensure that the lookup_output is not simultaneously assigned to by other non-lookup operations eg. in the case of composite ops.
    pub lookup_output: VarTensor,
    /// [Selectors] generated when configuring the layer. We use a BTreeMap as we expect to configure many base gates.
    pub selectors: BTreeMap<(BaseOp, usize), Selector>,
    /// [Selectors] generated when configuring the layer. We use a BTreeMap as we expect to configure many lookup ops.
    pub lookup_selectors: BTreeMap<(LookupOp, usize), Selector>,
    /// [Table]
    pub tables: BTreeMap<LookupOp, Table<F>>,
    /// Activate sanity checks
    pub check_mode: CheckMode,
    _marker: PhantomData<F>,
}

impl<F: FieldExt + TensorType> BaseConfig<F> {
    /// Returns a new [BaseConfig] with no inputs, no selectors, and no tables.
    pub fn dummy(col_size: usize) -> Self {
        Self {
            inputs: vec![VarTensor::dummy(col_size), VarTensor::dummy(col_size)],
            lookup_input: VarTensor::dummy(col_size),
            output: VarTensor::dummy(col_size),
            lookup_output: VarTensor::dummy(col_size),
            selectors: BTreeMap::new(),
            lookup_selectors: BTreeMap::new(),
            tables: BTreeMap::new(),
            check_mode: CheckMode::SAFE,
            _marker: PhantomData,
        }
    }

    /// Configures [BaseOp]s for a given [ConstraintSystem].
    /// # Arguments
    /// * `inputs` - The explicit inputs to the operations.
    /// * `output` - The variable representing the (currently singular) output of the operations.
    /// * `check_mode` - The variable representing the (currently singular) output of the operations.
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        inputs: &[VarTensor; 2],
        output: &VarTensor,
        check_mode: CheckMode,
        tol: i32,
    ) -> Self {
        // setup a selector per base op
        let mut selectors = BTreeMap::new();

        assert!(inputs[0].num_cols() == inputs[1].num_cols());
        assert!(inputs[0].num_cols() == output.num_cols());

        for i in 0..output.num_cols() {
            selectors.insert((BaseOp::Add, i), meta.selector());
            selectors.insert((BaseOp::Sub, i), meta.selector());
            selectors.insert((BaseOp::Dot, i), meta.selector());
            selectors.insert((BaseOp::Sum, i), meta.selector());
            selectors.insert((BaseOp::Neg, i), meta.selector());
            selectors.insert((BaseOp::Mult, i), meta.selector());
            selectors.insert((BaseOp::Range { tol }, i), meta.selector());
            selectors.insert((BaseOp::IsZero, i), meta.selector());
            selectors.insert((BaseOp::Identity, i), meta.selector());
            selectors.insert((BaseOp::IsBoolean, i), meta.selector());
        }

        // Given a range R and a value v, returns the expression
        // (v) * (1 - v) * (2 - v) * ... * (R - 1 - v)
        let range_check = |tol: i32, value: Expression<F>| {
            (-tol..tol).fold(value.clone(), |expr, i| {
                expr * (Expression::Constant(i32_to_felt(i)) - value.clone())
            })
        };

        for ((base_op, col_idx), selector) in selectors.iter() {
            meta.create_gate(base_op.as_str(), |meta| {
                let selector = meta.query_selector(*selector);
                let idx_offset = col_idx * output.col_size();
                let mut qis = vec![Expression::<F>::zero().unwrap(); 2];
                for (i, q_i) in qis
                    .iter_mut()
                    .enumerate()
                    .take(2)
                    .skip(2 - base_op.num_inputs())
                {
                    *q_i = inputs[i]
                        .query_rng(meta, 0, idx_offset, 1)
                        .expect("accum: input query failed")[0]
                        .clone()
                }

                // Get output expressions for each input channel
                let (rotation_offset, rng) = base_op.query_offset_rng();

                let constraints = match base_op {
                    BaseOp::Range { tol } => {
                        let expected_output: Tensor<Expression<F>> = output
                            .query_rng(meta, rotation_offset, idx_offset, rng)
                            .expect("poly: output query failed");

                        let res = qis[1].clone();
                        vec![range_check(
                            *tol,
                            res - expected_output[base_op.constraint_idx()].clone(),
                        )]
                    }
                    BaseOp::IsBoolean => {
                        vec![(qis[1].clone()) * (qis[1].clone() - Expression::Constant(F::from(1)))]
                    }
                    BaseOp::IsZero => vec![qis[1].clone()],
                    _ => {
                        let expected_output: Tensor<Expression<F>> = output
                            .query_rng(meta, rotation_offset, idx_offset, rng)
                            .expect("poly: output query failed");

                        let res =
                            base_op.f((qis[0].clone(), qis[1].clone(), expected_output[0].clone()));
                        vec![expected_output[base_op.constraint_idx()].clone() - res]
                    }
                };

                Constraints::with_selector(selector, constraints)
            });
        }

        let col = meta.fixed_column();
        meta.enable_constant(col);

        Self {
            selectors,
            lookup_selectors: BTreeMap::new(),
            inputs: inputs.to_vec(),
            lookup_input: VarTensor::Empty,
            lookup_output: VarTensor::Empty,
            tables: BTreeMap::new(),
            output: output.clone(),
            check_mode,
            _marker: PhantomData,
        }
    }

    /// Configures and creates lookup selectors
    pub fn configure_lookup(
        &mut self,
        cs: &mut ConstraintSystem<F>,
        input: &VarTensor,
        output: &VarTensor,
        bits: usize,
        nl: &LookupOp,
    ) -> Result<(), Box<dyn Error>> {
        let mut selectors = BTreeMap::new();
        let table =
            if let std::collections::btree_map::Entry::Vacant(e) = self.tables.entry(nl.clone()) {
                let table = Table::<F>::configure(cs, bits, nl);
                e.insert(table.clone());
                table
            } else {
                return Ok(());
            };
        for x in 0..input.num_cols() {
            let qlookup = cs.complex_selector();
            selectors.insert((nl.clone(), x), qlookup);
            let _ = cs.lookup(Op::<F>::as_str(nl), |cs| {
                let qlookup = cs.query_selector(qlookup);
                let not_qlookup = Expression::Constant(<F as Field>::one()) - qlookup.clone();
                let (default_x, default_y): (F, F) = nl.default_pair();
                vec![
                    (
                        match &input {
                            VarTensor::Advice { inner: advices, .. } => {
                                qlookup.clone() * cs.query_advice(advices[x], Rotation(0))
                                    + not_qlookup.clone() * default_x
                            }
                            VarTensor::Fixed { inner: fixed, .. } => {
                                qlookup.clone() * cs.query_fixed(fixed[x], Rotation(0))
                                    + not_qlookup.clone() * default_x
                            }
                            _ => panic!("uninitialized Vartensor"),
                        },
                        table.table_input,
                    ),
                    (
                        match &output {
                            VarTensor::Advice { inner: advices, .. } => {
                                qlookup * cs.query_advice(advices[x], Rotation(0))
                                    + not_qlookup * default_y
                            }
                            VarTensor::Fixed { inner: fixed, .. } => {
                                qlookup * cs.query_fixed(fixed[x], Rotation(0))
                                    + not_qlookup * default_y
                            }
                            _ => panic!("uninitialized Vartensor"),
                        },
                        table.table_output,
                    ),
                ]
            });
        }
        self.lookup_selectors.extend(selectors);
        // if we haven't previously initialized the input/output, do so now
        if let VarTensor::Empty = self.lookup_input {
            warn!("assiging lookup input");
            self.lookup_input = input.clone();
        }
        if let VarTensor::Empty = self.lookup_output {
            warn!("assiging lookup output");
            self.lookup_output = output.clone();
        }
        Ok(())
    }

    /// layout_tables must be called before layout.
    pub fn layout_tables(&mut self, layouter: &mut impl Layouter<F>) -> Result<(), Box<dyn Error>> {
        for table in self.tables.values_mut() {
            if !table.is_assigned {
                table.layout(layouter)?;
            }
        }
        Ok(())
    }

    /// Assigns variables to the regions created when calling `configure`.
    /// # Arguments
    /// * `values` - The explicit values to the operations.
    /// * `layouter` - A Halo2 Layouter.
    /// * `offset` - Offset to assign.
    /// * `op` - The operation being represented.
    pub fn layout(
        &mut self,
        mut region: Option<&mut Region<F>>,
        values: &[ValTensor<F>],
        offset: &mut usize,
        op: Box<dyn Op<F>>,
    ) -> Result<Option<ValTensor<F>>, Box<dyn Error>> {
        let mut cp_values = vec![];
        for v in values.iter() {
            if let ValTensor::Instance { .. } = v {
                cp_values.push(layouts::identity(
                    self,
                    region.as_deref_mut(),
                    &[v.clone()],
                    offset,
                )?);
            } else {
                cp_values.push(v.clone());
            }
        }
        let res = op.layout(self, region, &cp_values, offset);
        res
    }
}
