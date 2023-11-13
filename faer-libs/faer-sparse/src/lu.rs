use crate::{
    cholesky::{simplicial::EliminationTreeRef, supernodal::CholeskySymbolicSupernodalParams},
    ghost,
    ghost::{Array, MaybeIdx},
    mem::{
        NONE, {self},
    },
    try_zeroed, FaerError, Index, SymbolicSparseColMatRef,
};
use core::{iter::zip, mem::MaybeUninit};
use dyn_stack::PodStack;
use faer_core::{
    constrained::Size,
    group_helpers::{SliceGroup, SliceGroupMut},
    mul,
    permutation::{PermutationRef, SignedIndex},
    sparse::SparseColMatRef,
    Conj, MatMut, Parallelism,
};
use faer_entity::*;
use reborrow::*;

pub mod supernodal {
    use super::*;
    use faer_core::{assert, solve};

    #[inline(never)]
    fn resize_scalar<E: Entity>(
        v: &mut GroupFor<E, alloc::vec::Vec<E::Unit>>,
        n: usize,
        exact: bool,
        reserve_only: bool,
    ) -> Result<(), FaerError> {
        let mut failed = false;
        let reserve = if exact {
            alloc::vec::Vec::try_reserve_exact
        } else {
            alloc::vec::Vec::try_reserve
        };

        E::faer_map(E::faer_as_mut(v), |v| {
            if !failed {
                failed = reserve(v, n.saturating_sub(v.len())).is_err();
                if !reserve_only {
                    v.resize(Ord::max(n, v.len()), unsafe { core::mem::zeroed() });
                }
            }
        });
        if failed {
            Err(FaerError::OutOfMemory)
        } else {
            Ok(())
        }
    }

    #[inline(never)]
    fn resize_maybe_uninit_scalar<E: Entity>(
        v: &mut GroupFor<E, alloc::vec::Vec<MaybeUninit<E::Unit>>>,
        n: usize,
    ) -> Result<(), FaerError> {
        let mut failed = false;
        E::faer_map(E::faer_as_mut(v), |v| {
            if !failed {
                failed = v.try_reserve(n.saturating_sub(v.len())).is_err();
                unsafe { v.set_len(n) };
            }
        });
        if failed {
            Err(FaerError::OutOfMemory)
        } else {
            Ok(())
        }
    }

    #[inline(never)]
    fn resize_index<I: Index>(
        v: &mut alloc::vec::Vec<I>,
        n: usize,
        exact: bool,
        reserve_only: bool,
    ) -> Result<(), FaerError> {
        let reserve = if exact {
            alloc::vec::Vec::try_reserve_exact
        } else {
            alloc::vec::Vec::try_reserve
        };
        reserve(v, n.saturating_sub(v.len())).map_err(|_| FaerError::OutOfMemory)?;
        if !reserve_only {
            v.resize(Ord::max(n, v.len()), I::truncate(0));
        }
        Ok(())
    }

    #[derive(Copy, Clone, Debug)]
    pub enum LuError {
        Generic(FaerError),
        SymbolicSingular(usize),
    }

    impl From<FaerError> for LuError {
        #[inline]
        fn from(value: FaerError) -> Self {
            Self::Generic(value)
        }
    }

    #[inline]
    fn to_slice_group_mut<E: Entity>(
        v: &mut GroupFor<E, alloc::vec::Vec<E::Unit>>,
    ) -> SliceGroupMut<'_, E> {
        SliceGroupMut::<'_, E>::new(E::faer_map(E::faer_as_mut(v), |v| &mut **v))
    }
    #[inline]
    fn to_slice_group<E: Entity>(v: &GroupFor<E, alloc::vec::Vec<E::Unit>>) -> SliceGroup<'_, E> {
        SliceGroup::<'_, E>::new(E::faer_map(E::faer_as_ref(v), |v| &**v))
    }

    #[derive(Debug)]
    pub struct SymbolicSupernodalLu<I> {
        pub(super) supernode_ptr: alloc::vec::Vec<I>,
        pub(super) super_etree: alloc::vec::Vec<I>,
        pub(super) supernode_postorder: alloc::vec::Vec<I>,
        pub(super) supernode_postorder_inv: alloc::vec::Vec<I>,
        pub(super) descendant_count: alloc::vec::Vec<I>,
    }
    pub struct SupernodalLu<I, E: Entity> {
        nrows: usize,
        ncols: usize,
        nsupernodes: usize,

        supernode_ptr: alloc::vec::Vec<I>,

        l_col_ptr_for_row_ind: alloc::vec::Vec<I>,
        l_col_ptr_for_val: alloc::vec::Vec<I>,
        l_row_ind: alloc::vec::Vec<I>,
        l_val: GroupFor<E, alloc::vec::Vec<E::Unit>>,

        ut_col_ptr_for_row_ind: alloc::vec::Vec<I>,
        ut_col_ptr_for_val: alloc::vec::Vec<I>,
        ut_row_ind: alloc::vec::Vec<I>,
        ut_val: GroupFor<E, alloc::vec::Vec<E::Unit>>,
        // iwork: alloc::vec::Vec<I>,
        // work: GroupFor<E, alloc::vec::Vec<E::Unit>>,
    }
    unsafe impl<I: Index, E: Entity> Send for SupernodalLu<I, E> {}
    unsafe impl<I: Index, E: Entity> Sync for SupernodalLu<I, E> {}

    impl<I: Index, E: Entity> core::fmt::Debug for SupernodalLu<I, E> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("SupernodalLu")
                .field("nrows", &self.nrows)
                .field("ncols", &self.ncols)
                .field("nsupernodes", &self.nsupernodes)
                .field("l_col_ptr_for_row_ind", &self.l_col_ptr_for_row_ind)
                .field("l_col_ptr_for_val", &self.l_col_ptr_for_val)
                .field("l_row_ind", &self.l_row_ind)
                .field("l_val", &to_slice_group::<E>(&self.l_val))
                .field("ut_col_ptr_for_row_ind", &self.ut_col_ptr_for_row_ind)
                .field("ut_col_ptr_for_val", &self.ut_col_ptr_for_val)
                .field("ut_row_ind", &self.ut_row_ind)
                .field("ut_val", &to_slice_group::<E>(&self.ut_val))
                .finish()
        }
    }

    impl<I: Index, E: Entity> SupernodalLu<I, E> {
        #[inline]
        pub fn new() -> Self {
            Self {
                nrows: 0,
                ncols: 0,
                nsupernodes: 0,

                supernode_ptr: alloc::vec::Vec::new(),

                l_col_ptr_for_row_ind: alloc::vec::Vec::new(),
                ut_col_ptr_for_row_ind: alloc::vec::Vec::new(),

                l_col_ptr_for_val: alloc::vec::Vec::new(),
                ut_col_ptr_for_val: alloc::vec::Vec::new(),

                l_row_ind: alloc::vec::Vec::new(),
                ut_row_ind: alloc::vec::Vec::new(),

                l_val: E::faer_map(E::UNIT, |()| alloc::vec::Vec::<E::Unit>::new()),
                ut_val: E::faer_map(E::UNIT, |()| alloc::vec::Vec::<E::Unit>::new()),
                // iwork: alloc::vec::Vec::new(),
                // work: E::faer_map(E::UNIT, |()| alloc::vec::Vec::<E::Unit>::new()),
            }
        }

        #[inline]
        pub fn nrows(&self) -> usize {
            self.nrows
        }

        #[inline]
        pub fn ncols(&self) -> usize {
            self.ncols
        }

        #[inline]
        pub fn n_supernodes(&self) -> usize {
            self.nsupernodes
        }

        // #[track_caller]
        pub fn solve_in_place_with_conj(
            &self,
            row_perm: PermutationRef<'_, I, E>,
            col_perm: PermutationRef<'_, I, E>,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            assert!(self.nrows() == self.ncols());
            assert!(self.nrows() == rhs.nrows());
            let mut X = rhs;
            let (mut temp, mut stack) =
                faer_core::temp_mat_uninit::<E>(self.nrows(), X.ncols(), stack);

            faer_core::permutation::permute_rows(temp.rb_mut(), X.rb(), row_perm);
            self.l_solve_in_place_with_conj(conj_lhs, temp.rb_mut(), parallelism, stack.rb_mut());
            self.u_solve_in_place_with_conj(conj_lhs, temp.rb_mut(), parallelism, stack.rb_mut());
            faer_core::permutation::permute_rows(X.rb_mut(), temp.rb(), col_perm);
        }

        // #[track_caller]
        pub fn solve_transpose_in_place_with_conj(
            &self,
            row_perm: PermutationRef<'_, I, E>,
            col_perm: PermutationRef<'_, I, E>,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            assert!(self.nrows() == self.ncols());
            assert!(self.nrows() == rhs.nrows());
            let mut X = rhs;
            let (mut temp, mut stack) =
                faer_core::temp_mat_uninit::<E>(self.nrows(), X.ncols(), stack);
            faer_core::permutation::permute_rows(temp.rb_mut(), X.rb(), col_perm);
            self.u_solve_transpose_in_place_with_conj(
                conj_lhs,
                temp.rb_mut(),
                parallelism,
                stack.rb_mut(),
            );
            self.l_solve_transpose_in_place_with_conj(
                conj_lhs,
                temp.rb_mut(),
                parallelism,
                stack.rb_mut(),
            );
            faer_core::permutation::permute_rows(X.rb_mut(), temp.rb(), row_perm.inverse());
        }

        // #[track_caller]
        pub fn l_solve_in_place_with_conj(
            &self,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            let lu = &*self;

            assert!(lu.nrows() == lu.ncols());
            assert!(lu.nrows() == rhs.nrows());

            let (mut work, _) = faer_core::temp_mat_uninit::<E>(rhs.nrows(), rhs.ncols(), stack);

            let mut X = rhs;
            let nrhs = X.ncols();

            let supernode_ptr = &*lu.supernode_ptr;

            for s in 0..lu.nsupernodes {
                let s_begin = supernode_ptr[s].zx();
                let s_end = supernode_ptr[s + 1].zx();
                let s_size = s_end - s_begin;
                let s_row_index_count =
                    (lu.l_col_ptr_for_row_ind[s + 1] - lu.l_col_ptr_for_row_ind[s]).zx();

                let L = to_slice_group::<E>(&lu.l_val)
                    .subslice(lu.l_col_ptr_for_val[s].zx()..lu.l_col_ptr_for_val[s + 1].zx());
                let L = faer_core::mat::from_column_major_slice::<'_, E>(
                    L.into_inner(),
                    s_row_index_count,
                    s_size,
                );
                let (L_top, L_bot) = L.split_at_row(s_size);
                solve::solve_unit_lower_triangular_in_place_with_conj(
                    L_top,
                    conj_lhs,
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    parallelism,
                );
                mul::matmul_with_conj(
                    work.rb_mut().subrows_mut(0, s_row_index_count - s_size),
                    L_bot,
                    conj_lhs,
                    X.rb().subrows(s_begin, s_size),
                    Conj::No,
                    None,
                    E::faer_one(),
                    parallelism,
                );

                for j in 0..nrhs {
                    for (idx, &i) in lu.l_row_ind
                        [lu.l_col_ptr_for_row_ind[s].zx()..lu.l_col_ptr_for_row_ind[s + 1].zx()]
                        [s_size..]
                        .iter()
                        .enumerate()
                    {
                        let i = i.zx();
                        X.write(i, j, X.read(i, j).faer_sub(work.read(idx, j)));
                    }
                }
            }
        }

        // #[track_caller]
        pub fn l_solve_transpose_in_place_with_conj(
            &self,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            let lu = &*self;

            assert!(lu.nrows() == lu.ncols());
            assert!(lu.nrows() == rhs.nrows());

            let (mut work, _) = faer_core::temp_mat_uninit::<E>(rhs.nrows(), rhs.ncols(), stack);

            let mut X = rhs;
            let nrhs = X.ncols();

            let supernode_ptr = &*lu.supernode_ptr;

            for s in (0..lu.nsupernodes).rev() {
                let s_begin = supernode_ptr[s].zx();
                let s_end = supernode_ptr[s + 1].zx();
                let s_size = s_end - s_begin;
                let s_row_index_count =
                    (lu.l_col_ptr_for_row_ind[s + 1] - lu.l_col_ptr_for_row_ind[s]).zx();

                let L = to_slice_group::<E>(&lu.l_val)
                    .subslice(lu.l_col_ptr_for_val[s].zx()..lu.l_col_ptr_for_val[s + 1].zx());
                let L = faer_core::mat::from_column_major_slice::<'_, E>(
                    L.into_inner(),
                    s_row_index_count,
                    s_size,
                );

                let (L_top, L_bot) = L.split_at_row(s_size);

                for j in 0..nrhs {
                    for (idx, &i) in lu.l_row_ind
                        [lu.l_col_ptr_for_row_ind[s].zx()..lu.l_col_ptr_for_row_ind[s + 1].zx()]
                        [s_size..]
                        .iter()
                        .enumerate()
                    {
                        let i = i.zx();
                        work.write(idx, j, X.read(i, j));
                    }
                }

                mul::matmul_with_conj(
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    L_bot.transpose(),
                    conj_lhs,
                    work.rb().subrows(0, s_row_index_count - s_size),
                    Conj::No,
                    Some(E::faer_one()),
                    E::faer_one().faer_neg(),
                    parallelism,
                );
                solve::solve_unit_upper_triangular_in_place_with_conj(
                    L_top.transpose(),
                    conj_lhs,
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    parallelism,
                );
            }
        }

        // #[track_caller]
        pub fn u_solve_in_place_with_conj(
            &self,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            let lu = &*self;

            assert!(lu.nrows() == lu.ncols());
            assert!(lu.nrows() == rhs.nrows());

            let (mut work, _) = faer_core::temp_mat_uninit::<E>(rhs.nrows(), rhs.ncols(), stack);

            let mut X = rhs;
            let nrhs = X.ncols();

            let supernode_ptr = &*lu.supernode_ptr;

            for s in (0..lu.nsupernodes).rev() {
                let s_begin = supernode_ptr[s].zx();
                let s_end = supernode_ptr[s + 1].zx();
                let s_size = s_end - s_begin;
                let s_row_index_count =
                    (lu.l_col_ptr_for_row_ind[s + 1] - lu.l_col_ptr_for_row_ind[s]).zx();
                let s_col_index_count =
                    (lu.ut_col_ptr_for_row_ind[s + 1] - lu.ut_col_ptr_for_row_ind[s]).zx();

                let L = to_slice_group::<E>(&lu.l_val)
                    .subslice(lu.l_col_ptr_for_val[s].zx()..lu.l_col_ptr_for_val[s + 1].zx());
                let L = faer_core::mat::from_column_major_slice::<'_, E>(
                    L.into_inner(),
                    s_row_index_count,
                    s_size,
                );
                let U = to_slice_group::<E>(&lu.ut_val)
                    .subslice(lu.ut_col_ptr_for_val[s].zx()..lu.ut_col_ptr_for_val[s + 1].zx());
                let U_right = faer_core::mat::from_column_major_slice::<'_, E>(
                    U.into_inner(),
                    s_col_index_count,
                    s_size,
                )
                .transpose();

                for j in 0..nrhs {
                    for (idx, &i) in lu.ut_row_ind
                        [lu.ut_col_ptr_for_row_ind[s].zx()..lu.ut_col_ptr_for_row_ind[s + 1].zx()]
                        .iter()
                        .enumerate()
                    {
                        let i = i.zx();
                        work.write(idx, j, X.read(i, j));
                    }
                }

                let (U_left, _) = L.split_at_row(s_size);
                mul::matmul_with_conj(
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    U_right,
                    conj_lhs,
                    work.rb().subrows(0, s_col_index_count),
                    Conj::No,
                    Some(E::faer_one()),
                    E::faer_one().faer_neg(),
                    parallelism,
                );
                solve::solve_upper_triangular_in_place_with_conj(
                    U_left,
                    conj_lhs,
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    parallelism,
                );
            }
        }

        // #[track_caller]
        pub fn u_solve_transpose_in_place_with_conj(
            &self,
            conj_lhs: Conj,
            rhs: MatMut<'_, E>,
            parallelism: Parallelism,
            stack: PodStack<'_>,
        ) where
            E: ComplexField,
        {
            let lu = &*self;

            assert!(lu.nrows() == lu.ncols());
            assert!(lu.nrows() == rhs.nrows());

            let (mut work, _) = faer_core::temp_mat_uninit::<E>(rhs.nrows(), rhs.ncols(), stack);

            let mut X = rhs;
            let nrhs = X.ncols();

            let supernode_ptr = &*lu.supernode_ptr;

            for s in 0..lu.nsupernodes {
                let s_begin = supernode_ptr[s].zx();
                let s_end = supernode_ptr[s + 1].zx();
                let s_size = s_end - s_begin;
                let s_row_index_count =
                    (lu.l_col_ptr_for_row_ind[s + 1] - lu.l_col_ptr_for_row_ind[s]).zx();
                let s_col_index_count =
                    (lu.ut_col_ptr_for_row_ind[s + 1] - lu.ut_col_ptr_for_row_ind[s]).zx();

                let L = to_slice_group::<E>(&lu.l_val)
                    .subslice(lu.l_col_ptr_for_val[s].zx()..lu.l_col_ptr_for_val[s + 1].zx());
                let L = faer_core::mat::from_column_major_slice::<'_, E>(
                    L.into_inner(),
                    s_row_index_count,
                    s_size,
                );
                let U = to_slice_group::<E>(&lu.ut_val)
                    .subslice(lu.ut_col_ptr_for_val[s].zx()..lu.ut_col_ptr_for_val[s + 1].zx());
                let U_right = faer_core::mat::from_column_major_slice::<'_, E>(
                    U.into_inner(),
                    s_col_index_count,
                    s_size,
                )
                .transpose();

                let (U_left, _) = L.split_at_row(s_size);
                solve::solve_lower_triangular_in_place_with_conj(
                    U_left.transpose(),
                    conj_lhs,
                    X.rb_mut().subrows_mut(s_begin, s_size),
                    parallelism,
                );
                mul::matmul_with_conj(
                    work.rb_mut().subrows_mut(0, s_col_index_count),
                    U_right.transpose(),
                    conj_lhs,
                    X.rb().subrows(s_begin, s_size),
                    Conj::No,
                    None,
                    E::faer_one(),
                    parallelism,
                );

                for j in 0..nrhs {
                    for (idx, &i) in lu.ut_row_ind
                        [lu.ut_col_ptr_for_row_ind[s].zx()..lu.ut_col_ptr_for_row_ind[s + 1].zx()]
                        .iter()
                        .enumerate()
                    {
                        let i = i.zx();
                        X.write(i, j, X.read(i, j).faer_sub(work.read(idx, j)));
                    }
                }
            }
        }
    }

    #[track_caller]
    pub fn factorize_supernodal_symbolic<I: Index>(
        A: SymbolicSparseColMatRef<'_, I>,
        col_perm: Option<PermutationRef<'_, I, Symbolic>>,
        min_col: &[I],
        etree: EliminationTreeRef<'_, I>,
        col_counts: &[I],
        stack: PodStack<'_>,
        params: CholeskySymbolicSupernodalParams<'_>,
    ) -> Result<SymbolicSupernodalLu<I>, FaerError> {
        let m = A.nrows();
        let n = A.ncols();
        Size::with2(m, n, |M, N| {
            let I = I::truncate;
            let A = ghost::SymbolicSparseColMatRef::new(A, M, N);
            let min_col = Array::from_ref(
                MaybeIdx::from_slice_ref_checked(bytemuck::cast_slice(&min_col), N),
                M,
            );
            let etree = etree.ghost_inner(N);
            let mut stack = stack;

            let L = crate::cholesky::supernodal::ghost_factorize_supernodal_symbolic(
                A,
                col_perm.map(|perm| ghost::PermutationRef::new(perm, N)),
                Some(min_col),
                crate::cholesky::supernodal::CholeskyInput::ATA,
                etree,
                Array::from_ref(&col_counts, N),
                stack.rb_mut(),
                params,
            )?;
            let n_supernodes = L.n_supernodes();
            let mut super_etree = try_zeroed::<I>(n_supernodes)?;

            let (index_to_super, _) = stack.make_raw::<I>(*N);

            for s in 0..n_supernodes {
                index_to_super.as_mut()[L.supernode_begin[s].zx()..L.supernode_begin[s + 1].zx()]
                    .fill(I(s));
            }
            for s in 0..n_supernodes {
                let last = L.supernode_begin[s + 1].zx() - 1;
                if let Some(parent) = etree[N.check(last)].idx() {
                    super_etree[s] = index_to_super[*parent.zx()];
                } else {
                    super_etree[s] = I(NONE);
                }
            }

            Ok(SymbolicSupernodalLu {
                supernode_ptr: L.supernode_begin,
                super_etree,
                supernode_postorder: L.supernode_postorder,
                supernode_postorder_inv: L.supernode_postorder_inv,
                descendant_count: L.descendant_count,
            })
        })
    }

    struct MatU8 {
        data: alloc::vec::Vec<u8>,
        nrows: usize,
    }
    impl MatU8 {
        fn new(nrows: usize, ncols: usize) -> Self {
            Self {
                data: alloc::vec![1u8; nrows * ncols],
                nrows,
            }
        }
    }
    impl core::ops::Index<(usize, usize)> for MatU8 {
        type Output = u8;
        #[inline(always)]
        fn index(&self, (row, col): (usize, usize)) -> &Self::Output {
            &self.data[row + col * self.nrows]
        }
    }
    impl core::ops::IndexMut<(usize, usize)> for MatU8 {
        #[inline(always)]
        fn index_mut(&mut self, (row, col): (usize, usize)) -> &mut Self::Output {
            &mut self.data[row + col * self.nrows]
        }
    }

    struct Front;
    struct LPanel;
    struct UPanel;

    #[inline(never)]
    fn noinline<T, R>(_: T, f: impl FnOnce() -> R) -> R {
        f()
    }

    pub fn factorize_supernodal_numeric_lu<I: Index, E: ComplexField>(
        row_perm: &mut [I],
        row_perm_inv: &mut [I],
        lu: &mut SupernodalLu<I, E>,

        A: SparseColMatRef<'_, I, E>,
        AT: SparseColMatRef<'_, I, E>,
        col_perm: PermutationRef<'_, I, E>,
        symbolic: &SymbolicSupernodalLu<I>,

        parallelism: Parallelism,
        stack: PodStack<'_>,
    ) -> Result<(), LuError> {
        use crate::cholesky::supernodal::partition_fn;
        let SymbolicSupernodalLu {
            supernode_ptr,
            super_etree,
            supernode_postorder,
            supernode_postorder_inv,
            descendant_count,
        } = symbolic;

        let I = I::truncate;
        let I_checked = |x: usize| -> Result<I, FaerError> {
            if x > I::Signed::MAX.zx() {
                Err(FaerError::IndexOverflow)
            } else {
                Ok(I(x))
            }
        };
        let to_wide = |x: I| -> u128 { x.zx() as _ };
        let from_wide_checked = |x: u128| -> Result<I, FaerError> {
            if x > I::Signed::MAX.zx() as u128 {
                Err(FaerError::IndexOverflow)
            } else {
                Ok(I(x as _))
            }
        };

        let to_slice_group_mut = to_slice_group_mut::<E>;

        let m = A.nrows();
        let n = A.ncols();
        assert!(m >= n);
        assert!(AT.nrows() == n);
        assert!(AT.ncols() == m);
        assert!(row_perm.len() == m);
        assert!(row_perm_inv.len() == m);
        let n_supernodes = super_etree.len();
        assert!(supernode_postorder.len() == n_supernodes);
        assert!(supernode_postorder_inv.len() == n_supernodes);
        assert!(supernode_ptr.len() == n_supernodes + 1);
        assert!(supernode_ptr[n_supernodes].zx() == n);

        lu.nrows = 0;
        lu.ncols = 0;
        lu.nsupernodes = 0;
        lu.supernode_ptr.clear();

        let (col_global_to_local, stack) = stack.make_raw::<I>(n);
        let (row_global_to_local, stack) = stack.make_raw::<I>(m);
        let (marked, stack) = stack.make_raw::<I>(m);
        let (indices, stack) = stack.make_raw::<I>(m);
        let (transpositions, stack) = stack.make_raw::<I>(m);
        let (d_active_rows, _) = stack.make_raw::<I>(m);

        mem::fill_none::<I::Signed>(bytemuck::cast_slice_mut(col_global_to_local));
        mem::fill_none::<I::Signed>(bytemuck::cast_slice_mut(row_global_to_local));

        mem::fill_zero(marked);

        resize_index(&mut lu.l_col_ptr_for_row_ind, n_supernodes + 1, true, false)?;
        resize_index(
            &mut lu.ut_col_ptr_for_row_ind,
            n_supernodes + 1,
            true,
            false,
        )?;
        resize_index(&mut lu.l_col_ptr_for_val, n_supernodes + 1, true, false)?;
        resize_index(&mut lu.ut_col_ptr_for_val, n_supernodes + 1, true, false)?;

        lu.l_col_ptr_for_row_ind[0] = I(0);
        lu.ut_col_ptr_for_row_ind[0] = I(0);
        lu.l_col_ptr_for_val[0] = I(0);
        lu.ut_col_ptr_for_val[0] = I(0);

        for i in 0..m {
            row_perm[i] = I(i);
        }
        for i in 0..m {
            row_perm_inv[i] = I(i);
        }

        let (col_perm, col_perm_inv) = col_perm.into_arrays();

        let mut contrib_work = (0..n_supernodes)
            .map(|_| {
                (
                    E::faer_map(E::UNIT, |()| alloc::vec::Vec::<MaybeUninit<E::Unit>>::new()),
                    alloc::vec::Vec::<I>::new(),
                    0usize,
                    MatU8::new(0, 0),
                )
            })
            .collect::<alloc::vec::Vec<_>>();

        let work_is_empty = |v: &GroupFor<E, alloc::vec::Vec<MaybeUninit<E::Unit>>>| {
            let mut is_empty = false;
            E::faer_map(E::faer_as_ref(v), |v| is_empty |= v.is_empty());
            is_empty
        };

        let work_make_empty = |v: &mut GroupFor<E, alloc::vec::Vec<MaybeUninit<E::Unit>>>| {
            E::faer_map(E::faer_as_mut(v), |v| *v = alloc::vec::Vec::new());
        };

        let work_to_mat_mut = |v: &mut GroupFor<E, alloc::vec::Vec<MaybeUninit<E::Unit>>>,
                               nrows: usize,
                               ncols: usize| unsafe {
            faer_core::mat::from_raw_parts_mut::<'_, E>(
                E::faer_map(E::faer_as_mut(v), |v| v.as_mut_ptr() as *mut E::Unit),
                nrows,
                ncols,
                1,
                nrows as isize,
            )
        };

        let mut A_leftover = A.compute_nnz();
        for s in 0..n_supernodes {
            let s_begin = supernode_ptr[s].zx();
            let s_end = supernode_ptr[s + 1].zx();
            let s_size = s_end - s_begin;

            let s_postordered = supernode_postorder_inv[s].zx();
            let desc_count = descendant_count[s].zx();
            let mut s_row_index_count = 0usize;
            let (left_contrib, right_contrib) = contrib_work.split_at_mut(s);

            let s_row_indices = &mut *indices;
            // add the rows from A[s_end:, s_begin:s_end]
            for j in s_begin..s_end {
                let pj = col_perm[j].zx();
                let row_ind = A.row_indices_of_col_raw(pj);
                for i in row_ind {
                    let i = i.zx();
                    let pi = row_perm_inv[i].zx();
                    if pi < s_begin {
                        continue;
                    }
                    if marked[i] < I(2 * s + 1) {
                        s_row_indices[s_row_index_count] = I(i);
                        s_row_index_count += 1;
                        marked[i] = I(2 * s + 1);
                    }
                }
            }

            // add the rows from child[s_begin:]
            for d in &supernode_postorder[s_postordered - desc_count..s_postordered] {
                let d = d.zx();
                let d_begin = supernode_ptr[d].zx();
                let d_end = supernode_ptr[d + 1].zx();
                let d_size = d_end - d_begin;
                let d_row_ind = &lu.l_row_ind
                    [lu.l_col_ptr_for_row_ind[d].zx()..lu.l_col_ptr_for_row_ind[d + 1].zx()]
                    [d_size..];
                let d_col_ind = &lu.ut_row_ind
                    [lu.ut_col_ptr_for_row_ind[d].zx()..lu.ut_col_ptr_for_row_ind[d + 1].zx()];
                let d_col_start = d_col_ind.partition_point(partition_fn(s_begin));

                if d_col_start < d_col_ind.len() && d_col_ind[d_col_start].zx() < s_end {
                    for i in d_row_ind.iter() {
                        let i = i.zx();
                        let pi = row_perm_inv[i].zx();

                        if pi < s_begin {
                            continue;
                        }

                        if marked[i] < I(2 * s + 1) {
                            s_row_indices[s_row_index_count] = I(i);
                            s_row_index_count += 1;
                            marked[i] = I(2 * s + 1);
                        }
                    }
                }
            }

            lu.l_col_ptr_for_row_ind[s + 1] =
                I_checked(lu.l_col_ptr_for_row_ind[s].zx() + s_row_index_count)?;
            lu.l_col_ptr_for_val[s + 1] = from_wide_checked(
                to_wide(lu.l_col_ptr_for_val[s]) + ((s_row_index_count) as u128 * s_size as u128),
            )?;
            resize_index(
                &mut lu.l_row_ind,
                lu.l_col_ptr_for_row_ind[s + 1].zx(),
                false,
                false,
            )?;
            resize_scalar::<E>(
                &mut lu.l_val,
                lu.l_col_ptr_for_val[s + 1].zx(),
                false,
                false,
            )?;
            lu.l_row_ind[lu.l_col_ptr_for_row_ind[s].zx()..lu.l_col_ptr_for_row_ind[s + 1].zx()]
                .copy_from_slice(&s_row_indices[..s_row_index_count]);
            lu.l_row_ind[lu.l_col_ptr_for_row_ind[s].zx()..lu.l_col_ptr_for_row_ind[s + 1].zx()]
                .sort_unstable();

            let (left_row_indices, right_row_indices) =
                lu.l_row_ind.split_at_mut(lu.l_col_ptr_for_row_ind[s].zx());

            let s_row_indices = &mut right_row_indices
                [0..lu.l_col_ptr_for_row_ind[s + 1].zx() - lu.l_col_ptr_for_row_ind[s].zx()];
            for (idx, i) in s_row_indices.iter().enumerate() {
                row_global_to_local[i.zx()] = I(idx);
            }
            let s_L = to_slice_group_mut(&mut lu.l_val)
                .subslice(lu.l_col_ptr_for_val[s].zx()..lu.l_col_ptr_for_val[s + 1].zx());
            let mut s_L = faer_core::mat::from_column_major_slice_mut::<'_, E>(
                s_L.into_inner(),
                s_row_index_count,
                s_size,
            );
            s_L.fill_zero();

            for j in s_begin..s_end {
                let pj = col_perm[j].zx();
                let row_ind = A.row_indices_of_col(pj);
                let val = SliceGroup::<'_, E>::new(A.values_of_col(pj)).into_ref_iter();

                for (i, val) in zip(row_ind, val) {
                    let pi = row_perm_inv[i].zx();
                    let val = val.read();
                    if pi < s_begin {
                        continue;
                    }
                    assert!(A_leftover > 0);
                    A_leftover -= 1;
                    s_L.write(row_global_to_local[i].zx(), j - s_begin, val);
                }
            }

            noinline(LPanel, || {
                for d in &supernode_postorder[s_postordered - desc_count..s_postordered] {
                    let d = d.zx();
                    if work_is_empty(&left_contrib[d].0) {
                        continue;
                    }

                    let d_begin = supernode_ptr[d].zx();
                    let d_end = supernode_ptr[d + 1].zx();
                    let d_size = d_end - d_begin;
                    let d_row_ind = &left_row_indices
                        [lu.l_col_ptr_for_row_ind[d].zx()..lu.l_col_ptr_for_row_ind[d + 1].zx()]
                        [d_size..];
                    let d_col_ind = &lu.ut_row_ind
                        [lu.ut_col_ptr_for_row_ind[d].zx()..lu.ut_col_ptr_for_row_ind[d + 1].zx()];
                    let d_col_start = d_col_ind.partition_point(partition_fn(s_begin));

                    if d_col_start < d_col_ind.len() && d_col_ind[d_col_start].zx() < s_end {
                        let d_col_mid = d_col_start
                            + d_col_ind[d_col_start..].partition_point(partition_fn(s_end));

                        let mut d_LU_cols = work_to_mat_mut(
                            &mut left_contrib[d].0,
                            d_row_ind.len(),
                            d_col_ind.len(),
                        )
                        .subcols_mut(d_col_start, d_col_mid - d_col_start);
                        let d_active = &mut left_contrib[d].1[d_col_start..];
                        let d_active_count = &mut left_contrib[d].2;
                        let d_active_mat = &mut left_contrib[d].3;

                        for (d_j, j) in d_col_ind[d_col_start..d_col_mid].iter().enumerate() {
                            if d_active[d_j] > I(0) {
                                let mut taken_rows = 0usize;
                                let j = j.zx();
                                let s_j = j - s_begin;
                                for (d_i, i) in d_row_ind.iter().enumerate() {
                                    let i = i.zx();
                                    let pi = row_perm_inv[i].zx();
                                    if pi < s_begin {
                                        continue;
                                    }
                                    let s_i = row_global_to_local[i].zx();

                                    s_L.write(
                                        s_i,
                                        s_j,
                                        s_L.read(s_i, s_j).faer_sub(d_LU_cols.read(d_i, d_j)),
                                    );
                                    d_LU_cols.write(d_i, d_j, E::faer_zero());
                                    taken_rows += d_active_mat[(d_i, d_j + d_col_start)] as usize;
                                    d_active_mat[(d_i, d_j + d_col_start)] = 0;
                                }
                                assert!(d_active[d_j] >= I(taken_rows));
                                d_active[d_j] -= I(taken_rows);
                                if d_active[d_j] == I(0) {
                                    assert!(*d_active_count > 0);
                                    *d_active_count -= 1;
                                }
                            }
                        }
                        if *d_active_count == 0 {
                            work_make_empty(&mut left_contrib[d].0);
                            left_contrib[d].1 = alloc::vec::Vec::new();
                            left_contrib[d].2 = 0;
                            left_contrib[d].3 = MatU8::new(0, 0);
                        }
                    }
                }
            });

            if s_L.nrows() < s_L.ncols() {
                return Err(LuError::SymbolicSingular(s_begin + s_L.nrows()));
            }
            assert!(s_L.nrows() >= s_L.ncols());
            let transpositions = &mut transpositions[s_begin..s_end];
            faer_lu::partial_pivoting::compute::lu_in_place_impl(
                s_L.rb_mut(),
                0,
                s_size,
                transpositions,
                parallelism,
            );
            for (idx, t) in transpositions.iter().enumerate() {
                let i_t = s_row_indices[idx + t.zx()].zx();
                let kk = row_perm_inv[i_t].zx();
                row_perm.swap(s_begin + idx, row_perm_inv[i_t].zx());
                row_perm_inv.swap(row_perm[s_begin + idx].zx(), row_perm[kk].zx());
                s_row_indices.swap(idx, idx + t.zx());
            }
            for (idx, t) in transpositions.iter().enumerate().rev() {
                row_global_to_local.swap(s_row_indices[idx].zx(), s_row_indices[idx + t.zx()].zx());
            }
            for (idx, i) in s_row_indices.iter().enumerate() {
                assert!(row_global_to_local[i.zx()] == I(idx));
            }

            let s_col_indices = &mut indices[..n];
            let mut s_col_index_count = 0usize;
            for i in s_begin..s_end {
                let pi = row_perm[i].zx();
                for j in AT.row_indices_of_col(pi) {
                    let pj = col_perm_inv[j].zx();
                    if pj < s_end {
                        continue;
                    }
                    if marked[pj] < I(2 * s + 2) {
                        s_col_indices[s_col_index_count] = I(pj);
                        s_col_index_count += 1;
                        marked[pj] = I(2 * s + 2);
                    }
                }
            }

            for d in &supernode_postorder[s_postordered - desc_count..s_postordered] {
                let d = d.zx();

                let d_begin = supernode_ptr[d].zx();
                let d_end = supernode_ptr[d + 1].zx();
                let d_size = d_end - d_begin;

                let d_row_ind = &left_row_indices
                    [lu.l_col_ptr_for_row_ind[d].zx()..lu.l_col_ptr_for_row_ind[d + 1].zx()]
                    [d_size..];
                let d_col_ind = &lu.ut_row_ind
                    [lu.ut_col_ptr_for_row_ind[d].zx()..lu.ut_col_ptr_for_row_ind[d + 1].zx()];

                let contributes_to_u = d_row_ind.iter().any(|&i| {
                    row_perm_inv[i.zx()].zx() >= s_begin && row_perm_inv[i.zx()].zx() < s_end
                });

                if contributes_to_u {
                    let d_col_start = d_col_ind.partition_point(partition_fn(s_end));
                    for j in &d_col_ind[d_col_start..] {
                        let j = j.zx();
                        if marked[j] < I(2 * s + 2) {
                            s_col_indices[s_col_index_count] = I(j);
                            s_col_index_count += 1;
                            marked[j] = I(2 * s + 2);
                        }
                    }
                }
            }

            lu.ut_col_ptr_for_row_ind[s + 1] =
                I_checked(lu.ut_col_ptr_for_row_ind[s].zx() + s_col_index_count)?;
            lu.ut_col_ptr_for_val[s + 1] = from_wide_checked(
                to_wide(lu.ut_col_ptr_for_val[s]) + (s_col_index_count as u128 * s_size as u128),
            )?;
            resize_index(
                &mut lu.ut_row_ind,
                lu.ut_col_ptr_for_row_ind[s + 1].zx(),
                false,
                false,
            )?;
            resize_scalar::<E>(
                &mut lu.ut_val,
                lu.ut_col_ptr_for_val[s + 1].zx(),
                false,
                false,
            )?;
            lu.ut_row_ind[lu.ut_col_ptr_for_row_ind[s].zx()..lu.ut_col_ptr_for_row_ind[s + 1].zx()]
                .copy_from_slice(&s_col_indices[..s_col_index_count]);
            lu.ut_row_ind[lu.ut_col_ptr_for_row_ind[s].zx()..lu.ut_col_ptr_for_row_ind[s + 1].zx()]
                .sort_unstable();

            let s_col_indices = &lu.ut_row_ind
                [lu.ut_col_ptr_for_row_ind[s].zx()..lu.ut_col_ptr_for_row_ind[s + 1].zx()];
            for (idx, j) in s_col_indices.iter().enumerate() {
                col_global_to_local[j.zx()] = I(idx);
            }

            let s_U = to_slice_group_mut(&mut lu.ut_val)
                .subslice(lu.ut_col_ptr_for_val[s].zx()..lu.ut_col_ptr_for_val[s + 1].zx());
            let mut s_U = faer_core::mat::from_column_major_slice_mut::<'_, E>(
                s_U.into_inner(),
                s_col_index_count,
                s_size,
            )
            .transpose_mut();
            s_U.fill_zero();

            for i in s_begin..s_end {
                let pi = row_perm[i].zx();
                for (j, val) in zip(
                    AT.row_indices_of_col(pi),
                    SliceGroup::<'_, E>::new(AT.values_of_col(pi)).into_ref_iter(),
                ) {
                    let pj = col_perm_inv[j].zx();
                    let val = val.read();
                    if pj < s_end {
                        continue;
                    }
                    assert!(A_leftover > 0);
                    A_leftover -= 1;
                    s_U.write(i - s_begin, col_global_to_local[pj].zx(), val);
                }
            }

            noinline(UPanel, || {
                for d in &supernode_postorder[s_postordered - desc_count..s_postordered] {
                    let d = d.zx();
                    if work_is_empty(&left_contrib[d].0) {
                        continue;
                    }

                    let d_begin = supernode_ptr[d].zx();
                    let d_end = supernode_ptr[d + 1].zx();
                    let d_size = d_end - d_begin;

                    let d_row_ind = &left_row_indices
                        [lu.l_col_ptr_for_row_ind[d].zx()..lu.l_col_ptr_for_row_ind[d + 1].zx()]
                        [d_size..];
                    let d_col_ind = &lu.ut_row_ind
                        [lu.ut_col_ptr_for_row_ind[d].zx()..lu.ut_col_ptr_for_row_ind[d + 1].zx()];

                    let contributes_to_u = d_row_ind.iter().any(|&i| {
                        row_perm_inv[i.zx()].zx() >= s_begin && row_perm_inv[i.zx()].zx() < s_end
                    });

                    if contributes_to_u {
                        let d_col_start = d_col_ind.partition_point(partition_fn(s_end));
                        let d_LU = work_to_mat_mut(
                            &mut left_contrib[d].0,
                            d_row_ind.len(),
                            d_col_ind.len(),
                        );
                        let mut d_LU = d_LU.get_mut(.., d_col_start..);
                        let d_active = &mut left_contrib[d].1[d_col_start..];
                        let d_active_count = &mut left_contrib[d].2;
                        let d_active_mat = &mut left_contrib[d].3;

                        for (d_j, j) in d_col_ind[d_col_start..].iter().enumerate() {
                            if d_active[d_j] > I(0) {
                                let mut taken_rows = 0usize;
                                let j = j.zx();
                                let s_j = col_global_to_local[j].zx();
                                for (d_i, i) in d_row_ind.iter().enumerate() {
                                    let i = i.zx();
                                    let pi = row_perm_inv[i].zx();
                                    if pi >= s_begin && pi < s_end {
                                        let s_i = row_global_to_local[i].zx();
                                        s_U.write(
                                            s_i,
                                            s_j,
                                            s_U.read(s_i, s_j).faer_sub(d_LU.read(d_i, d_j)),
                                        );
                                        d_LU.write(d_i, d_j, E::faer_zero());
                                        taken_rows +=
                                            d_active_mat[(d_i, d_j + d_col_start)] as usize;
                                        d_active_mat[(d_i, d_j + d_col_start)] = 0;
                                    }
                                }
                                assert!(d_active[d_j] >= I(taken_rows));
                                d_active[d_j] -= I(taken_rows);
                                if d_active[d_j] == I(0) {
                                    assert!(*d_active_count > 0);
                                    *d_active_count -= 1;
                                }
                            }
                        }
                        if *d_active_count == 0 {
                            work_make_empty(&mut left_contrib[d].0);
                            left_contrib[d].1 = alloc::vec::Vec::new();
                            left_contrib[d].2 = 0;
                            left_contrib[d].3 = MatU8::new(0, 0);
                        }
                    }
                }
            });
            faer_core::solve::solve_unit_lower_triangular_in_place(
                s_L.rb().subrows(0, s_size),
                s_U.rb_mut(),
                parallelism,
            );

            if s_row_index_count > s_size && s_col_index_count > 0 {
                resize_maybe_uninit_scalar::<E>(
                    &mut right_contrib[0].0,
                    from_wide_checked(
                        to_wide(I(s_row_index_count - s_size)) * to_wide(I(s_col_index_count)),
                    )?
                    .zx(),
                )?;
                right_contrib[0]
                    .1
                    .resize(s_col_index_count, I(s_row_index_count - s_size));
                right_contrib[0].2 = s_col_index_count;
                right_contrib[0].3 = MatU8::new(s_row_index_count - s_size, s_col_index_count);

                let mut s_LU = work_to_mat_mut(
                    &mut right_contrib[0].0,
                    s_row_index_count - s_size,
                    s_col_index_count,
                );
                mul::matmul(
                    s_LU.rb_mut(),
                    s_L.rb().get(s_size.., ..),
                    s_U.rb(),
                    None,
                    E::faer_one(),
                    parallelism,
                );

                noinline(Front, || {
                    for d in &supernode_postorder[s_postordered - desc_count..s_postordered] {
                        let d = d.zx();
                        if work_is_empty(&left_contrib[d].0) {
                            continue;
                        }

                        let d_begin = supernode_ptr[d].zx();
                        let d_end = supernode_ptr[d + 1].zx();
                        let d_size = d_end - d_begin;

                        let d_row_ind = &left_row_indices[lu.l_col_ptr_for_row_ind[d].zx()
                            ..lu.l_col_ptr_for_row_ind[d + 1].zx()][d_size..];
                        let d_col_ind = &lu.ut_row_ind[lu.ut_col_ptr_for_row_ind[d].zx()
                            ..lu.ut_col_ptr_for_row_ind[d + 1].zx()];

                        let contributes_to_front = d_row_ind
                            .iter()
                            .any(|&i| row_perm_inv[i.zx()].zx() >= s_end);

                        if contributes_to_front {
                            let d_col_start = d_col_ind.partition_point(partition_fn(s_end));
                            let d_LU = work_to_mat_mut(
                                &mut left_contrib[d].0,
                                d_row_ind.len(),
                                d_col_ind.len(),
                            );
                            let mut d_LU = d_LU.get_mut(.., d_col_start..);
                            let d_active = &mut left_contrib[d].1[d_col_start..];
                            let d_active_count = &mut left_contrib[d].2;
                            let d_active_mat = &mut left_contrib[d].3;

                            let mut d_active_row_count = 0usize;
                            let mut first_iter = true;

                            for (d_j, j) in d_col_ind[d_col_start..].iter().enumerate() {
                                if d_active[d_j] > I(0) {
                                    if first_iter {
                                        first_iter = false;
                                        for (d_i, i) in d_row_ind.iter().enumerate() {
                                            let i = i.zx();
                                            let pi = row_perm_inv[i].zx();
                                            if (pi < s_end) || (row_global_to_local[i] == I(NONE)) {
                                                continue;
                                            }

                                            d_active_rows[d_active_row_count] = I(d_i);
                                            d_active_row_count += 1;
                                        }
                                    }

                                    let j = j.zx();
                                    let mut taken_rows = 0usize;

                                    let s_j = col_global_to_local[j];
                                    if s_j == I(NONE) {
                                        continue;
                                    }
                                    let s_j = s_j.zx();
                                    let mut dst = s_LU.rb_mut().col_mut(s_j);
                                    let mut src = d_LU.rb_mut().col_mut(d_j);
                                    assert!(dst.row_stride() == 1);
                                    assert!(src.row_stride() == 1);

                                    for d_i in &d_active_rows[..d_active_row_count] {
                                        let d_i = d_i.zx();
                                        let i = d_row_ind[d_i].zx();
                                        let d_active_mat =
                                            &mut d_active_mat[(d_i, d_j + d_col_start)];
                                        if *d_active_mat == 0 {
                                            continue;
                                        }
                                        let s_i = row_global_to_local[i].zx() - s_size;
                                        unsafe {
                                            dst.write_unchecked(
                                                s_i,
                                                dst.read_unchecked(s_i)
                                                    .faer_add(src.read_unchecked(d_i)),
                                            );
                                            src.write_unchecked(d_i, E::faer_zero());
                                        }
                                        taken_rows += 1;
                                        *d_active_mat = 0;
                                    }

                                    d_active[d_j] -= I(taken_rows);
                                    if d_active[d_j] == I(0) {
                                        *d_active_count -= 1;
                                    }
                                }
                            }
                            if *d_active_count == 0 {
                                work_make_empty(&mut left_contrib[d].0);
                                left_contrib[d].1 = alloc::vec::Vec::new();
                                left_contrib[d].2 = 0;
                                left_contrib[d].3 = MatU8::new(0, 0);
                            }
                        }
                    }
                })
            }

            for i in s_row_indices.iter() {
                row_global_to_local[i.zx()] = I(NONE);
            }
            for j in s_col_indices.iter() {
                col_global_to_local[j.zx()] = I(NONE);
            }
        }
        assert!(A_leftover == 0);

        for idx in &mut lu.l_row_ind[..lu.l_col_ptr_for_row_ind[n_supernodes].zx()] {
            *idx = row_perm_inv[idx.zx()];
        }

        lu.nrows = m;
        lu.ncols = n;
        lu.nsupernodes = n_supernodes;
        lu.supernode_ptr.clone_from(supernode_ptr);

        Ok(())
    }
}

#[cfg(test)]
#[cfg(__false)]
mod tests {
    use super::*;
    use crate::{
        lu::supernodal::{factorize_supernodal_numeric_lu, SupernodalLu},
        qr::col_etree,
        SymbolicSparseColMatRef,
    };
    use core::iter::zip;
    use dyn_stack::{GlobalPodBuffer, PodStack, StackReq};
    use faer_core::{
        assert,
        group_helpers::SliceGroup,
        permutation::{Index, PermutationRef},
        sparse::SparseColMatRef,
        Conj, Mat,
    };
    use faer_entity::{ComplexField, Symbolic};
    use matrix_market_rs::MtxData;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn sparse_to_dense<I: Index, E: ComplexField>(sparse: SparseColMatRef<'_, I, E>) -> Mat<E> {
        let m = sparse.nrows();
        let n = sparse.ncols();

        let mut dense = Mat::<E>::zeros(m, n);
        let slice_group = SliceGroup::<'_, E>::new;

        for j in 0..n {
            for (i, val) in zip(
                sparse.row_indices_of_col(j),
                slice_group(sparse.values_of_col(j)).into_ref_iter(),
            ) {
                dense.write(i, j, val.read());
            }
        }

        dense
    }

    fn load_mtx<I: Index>(
        data: MtxData<f64>,
    ) -> (
        usize,
        usize,
        alloc::vec::Vec<I>,
        alloc::vec::Vec<I>,
        alloc::vec::Vec<f64>,
    ) {
        let I = I::truncate;

        let MtxData::Sparse([nrows, ncols], coo_indices, coo_values, _) = data else {
            panic!()
        };

        let m = nrows;
        let n = ncols;
        let mut col_counts = vec![I(0); n];
        let mut col_ptr = vec![I(0); n + 1];

        for &[i, j] in &coo_indices {
            col_counts[j] += I(1);
            if i != j {
                col_counts[i] += I(1);
            }
        }

        for i in 0..n {
            col_ptr[i + 1] = col_ptr[i] + col_counts[i];
        }
        let nnz = col_ptr[n].zx();

        let mut row_ind = vec![I(0); nnz];
        let mut values = vec![0.0; nnz];

        col_counts.copy_from_slice(&col_ptr[..n]);

        for (&[i, j], &val) in zip(&coo_indices, &coo_values) {
            if i == j {
                values[col_counts[j].zx()] = 2.0 * val;
            } else {
                values[col_counts[i].zx()] = val;
                values[col_counts[j].zx()] = val;
            }

            row_ind[col_counts[j].zx()] = I(i);
            col_counts[j] += I(1);

            if i != j {
                row_ind[col_counts[i].zx()] = I(j);
                col_counts[i] += I(1);
            }
        }

        (m, n, col_ptr, row_ind, values)
    }

    fn naive_lu_perm(mut A: faer_core::MatMut<'_, f64>, p: &mut [usize], p_inv: &mut [usize]) {
        let m = A.nrows();
        let n = A.ncols();
        assert!(m >= n);

        for i in 0..m {
            p[i] = NONE;
            p_inv[i] = NONE;
        }

        let mut p2 = vec![0usize; m];
        let mut p_inv2 = vec![0usize; m];

        for i in 0..m {
            p2[i] = i;
            p_inv2[i] = i;
        }

        for k in 0..n {
            for j in 0..k {
                let jpiv = p[j];
                for i in k..m {
                    let i = p2[i];
                    let prod = A[(i, j)] * A[(jpiv, k)];
                    if prod != 0.0 {
                        dbg!(i, j, jpiv, k);
                        dbg!(A[(i, j)], A[(jpiv, k)], prod);
                    }
                    A[(i, k)] -= prod;
                }
            }
            dbgf::dbgf!("16.10?", &A);

            let mut max = -1.0;
            let mut kpiv = NONE;
            for i in 0..m {
                if p_inv[i] == NONE {
                    let val = A[(i, k)].abs();
                    if val > max {
                        max = val;
                        kpiv = i;
                    }
                }
            }

            dbgf::dbgf!("16.10?", A.rb().get(kpiv, k + 1..m).rb());
            for j in 0..k {
                let jpiv = p2[j];
                for jj in k + 1..m {
                    let prod = dbg!(A[(kpiv, j)]) * dbg!(A[(jpiv, jj)]);
                    dbg!(A[(kpiv, jj)]);
                    A[(kpiv, jj)] -= dbg!(prod);
                    dbg!(A[(kpiv, jj)]);
                }
            }
            dbgf::dbgf!("16.10?", A.rb().get(kpiv, k + 1..m).rb());

            p[k] = kpiv;
            p_inv[kpiv] = k;

            let kk = p_inv2[kpiv];
            p2.swap(k, kk);
            p_inv2.swap(p2[k], p2[kk]);
            dbgf::dbgf!("?", &p2, &p_inv2);
            dbg!(p2[k], p2[kk], p_inv2[kpiv]);
            dbgf::dbgf!("?", &p2, &p_inv2);

            for k in 0..n {
                assert!(p2[p_inv2[k]] == k);
            }

            let diag = A[(kpiv, k)];
            for i in 0..m {
                if p_inv[i] == NONE {
                    A[(i, k)] /= diag;
                }
            }

            // for j in k + 1..n {
            //     for i in 0..m {
            //         if p_inv[i] == NONE {
            //             let prod = A[(i, k)] * A[(piv, j)];
            //             A[(i, j)] -= prod;
            //         }
            //     }
            // }
        }
        let p = PermutationRef::<'_, usize, f64>::new_checked(p, p_inv);
        dbgf::dbgf!("16.10?", p * A.rb());
    }

    #[test]
    fn test_numeric_lu_multifrontal() {
        type E = faer_core::c64;

        let (m, n, col_ptr, row_ind, val) =
            load_mtx::<usize>(MtxData::from_file("bench_data/VALUES.mtx").unwrap());

        let mut rng = StdRng::seed_from_u64(0);
        let mut gen = || E::new(rng.gen::<f64>(), rng.gen::<f64>());

        let val = val.iter().map(|_| gen()).collect::<alloc::vec::Vec<_>>();
        let A = SparseColMatRef::<'_, usize, E>::new(
            SymbolicSparseColMatRef::new_checked(m, n, &col_ptr, None, &row_ind),
            &val,
        );
        let mut mem = GlobalPodBuffer::new(StackReq::new::<u8>(1024 * 1024 * 1024));

        let mut row_perm = vec![0usize; n];
        let mut row_perm_inv = vec![0usize; n];
        let mut col_perm = vec![0usize; n];
        let mut col_perm_inv = vec![0usize; n];
        for i in 0..n {
            col_perm[i] = i;
            col_perm_inv[i] = i;
        }
        let col_perm = PermutationRef::<'_, usize, Symbolic>::new_checked(&col_perm, &col_perm_inv);

        let mut etree = vec![0usize; n];
        let mut min_col = vec![0usize; m];
        let mut col_counts = vec![0usize; n];

        let nnz = A.compute_nnz();
        let mut new_col_ptrs = vec![0usize; m + 1];
        let mut new_row_ind = vec![0usize; nnz];
        let mut new_values = vec![E::faer_zero(); nnz];
        let AT = crate::transpose::<usize, E>(
            &mut new_col_ptrs,
            &mut new_row_ind,
            &mut new_values,
            A,
            PodStack::new(&mut mem),
        );

        let etree = {
            let mut post = vec![0usize; n];

            let etree = col_etree(*A, Some(col_perm), &mut etree, PodStack::new(&mut mem));
            crate::qr::postorder(&mut post, etree, PodStack::new(&mut mem));
            crate::qr::column_counts_aat(
                &mut col_counts,
                &mut min_col,
                *AT,
                Some(col_perm),
                etree,
                &post,
                PodStack::new(&mut mem),
            );
            etree
        };

        let symbolic = crate::lu::supernodal::factorize_supernodal_symbolic::<usize>(
            *A,
            Some(col_perm),
            &min_col,
            etree,
            &col_counts,
            PodStack::new(&mut mem),
            crate::cholesky::supernodal::CholeskySymbolicSupernodalParams {
                relax: Some(&[(4, 1.0), (16, 0.8), (48, 0.1), (usize::MAX, 0.05)]),
            },
        )
        .unwrap();

        let mut lu = SupernodalLu::<usize, E>::new();
        factorize_supernodal_numeric_lu(
            &mut row_perm,
            &mut row_perm_inv,
            &mut lu,
            A,
            AT,
            col_perm.cast(),
            &symbolic,
            faer_core::Parallelism::None,
            PodStack::new(&mut mem),
        )
        .unwrap();

        let k = 2;
        let rhs = Mat::from_fn(n, k, |_, _| gen());

        {
            let row_perm = PermutationRef::<'_, _, Symbolic>::new_checked(&row_perm, &row_perm_inv);
            let A_dense = sparse_to_dense(A);
            let mut x = rhs.clone();

            lu.solve_in_place_with_conj(
                row_perm.cast(),
                col_perm.cast(),
                Conj::No,
                x.as_mut(),
                faer_core::Parallelism::None,
                PodStack::new(&mut GlobalPodBuffer::new(StackReq::new::<usize>(
                    1024 * 1024,
                ))),
            );
            assert!((&A_dense * &x - &rhs).norm_max() < 1e-10);
        }
        {
            let row_perm = PermutationRef::<'_, _, Symbolic>::new_checked(&row_perm, &row_perm_inv);
            let A_dense = sparse_to_dense(A);
            let mut x = rhs.clone();

            lu.solve_in_place_with_conj(
                row_perm.cast(),
                col_perm.cast(),
                Conj::Yes,
                x.as_mut(),
                faer_core::Parallelism::None,
                PodStack::new(&mut GlobalPodBuffer::new(StackReq::new::<usize>(
                    1024 * 1024,
                ))),
            );
            assert!((A_dense.conjugate() * &x - &rhs).norm_max() < 1e-10);
        }
        {
            let row_perm = PermutationRef::<'_, _, Symbolic>::new_checked(&row_perm, &row_perm_inv);
            let A_dense = sparse_to_dense(A);
            let mut x = rhs.clone();

            lu.solve_transpose_in_place_with_conj(
                row_perm.cast(),
                col_perm.cast(),
                Conj::No,
                x.as_mut(),
                faer_core::Parallelism::None,
                PodStack::new(&mut GlobalPodBuffer::new(StackReq::new::<usize>(
                    1024 * 1024,
                ))),
            );
            assert!((A_dense.transpose() * &x - &rhs).norm_max() < 1e-10);
        }
        {
            let row_perm = PermutationRef::<'_, _, Symbolic>::new_checked(&row_perm, &row_perm_inv);
            let A_dense = sparse_to_dense(A);
            let mut x = rhs.clone();

            lu.solve_transpose_in_place_with_conj(
                row_perm.cast(),
                col_perm.cast(),
                Conj::Yes,
                x.as_mut(),
                faer_core::Parallelism::None,
                PodStack::new(&mut GlobalPodBuffer::new(StackReq::new::<usize>(
                    1024 * 1024,
                ))),
            );
            assert!((A_dense.adjoint() * &x - &rhs).norm_max() < 1e-10);
        }
    }
}
