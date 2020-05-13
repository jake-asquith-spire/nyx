use crate::dimensions::allocator::Allocator;
use crate::dimensions::{DefaultAllocator, DimName};

pub use super::estimate::*;
pub use super::kalman::*;
pub use super::ranging::*;
pub use super::residual::*;
pub use super::srif::*;
pub use super::*;

use crate::propagators::error_ctrl::ErrorCtrl;
use crate::propagators::Propagator;

use std::marker::PhantomData;
use std::sync::mpsc::Receiver;

pub struct ODProcess<
    'a,
    D: Estimable<MsrIn, LinStateSize = Msr::StateSize>,
    E: ErrorCtrl,
    Msr: Measurement,
    N: MeasurementDevice<Msr, MsrIn>,
    T: EkfTrigger,
    A: DimName,
    K: Filter<D::LinStateSize, A, Msr::MeasurementSize>,
    MsrIn,
> where
    DefaultAllocator: Allocator<f64, D::StateSize>
        + Allocator<f64, Msr::MeasurementSize>
        + Allocator<f64, Msr::MeasurementSize, Msr::StateSize>
        + Allocator<f64, D::LinStateSize>
        + Allocator<f64, Msr::MeasurementSize, Msr::MeasurementSize>
        + Allocator<f64, Msr::MeasurementSize, D::LinStateSize>
        + Allocator<f64, D::LinStateSize, Msr::MeasurementSize>
        + Allocator<f64, D::LinStateSize, D::LinStateSize>
        + Allocator<f64, A, A>
        + Allocator<f64, D::LinStateSize, A>
        + Allocator<f64, A, D::LinStateSize>,
{
    /// Propagator used for the estimation
    pub prop: &'a mut Propagator<'a, D, E>,
    /// Kalman filter itself
    pub kf: &'a mut K,
    /// List of measurement devices used
    pub devices: &'a [N],
    /// Whether or not these devices can make simultaneous measurements of the spacecraft
    pub simultaneous_msr: bool,
    /// Vector of estimates available after a pass
    pub estimates: Vec<K::Estimate>,
    /// Vector of residuals available after a pass
    pub residuals: Vec<Residual<Msr::MeasurementSize>>,
    pub ekf_trigger: T,
    _marker: PhantomData<A>,
}

impl<
        'a,
        D: Estimable<MsrIn, LinStateSize = Msr::StateSize>,
        E: ErrorCtrl,
        Msr: Measurement,
        N: MeasurementDevice<Msr, MsrIn>,
        T: EkfTrigger,
        A: DimName,
        K: Filter<D::LinStateSize, A, Msr::MeasurementSize>,
        MsrIn,
    > ODProcess<'a, D, E, Msr, N, T, A, K, MsrIn>
where
    DefaultAllocator: Allocator<f64, D::StateSize>
        + Allocator<f64, Msr::MeasurementSize>
        + Allocator<f64, Msr::MeasurementSize, Msr::StateSize>
        + Allocator<f64, D::LinStateSize>
        + Allocator<f64, Msr::MeasurementSize, Msr::MeasurementSize>
        + Allocator<f64, Msr::MeasurementSize, D::LinStateSize>
        + Allocator<f64, D::LinStateSize, Msr::MeasurementSize>
        + Allocator<f64, D::LinStateSize, D::LinStateSize>
        + Allocator<f64, A, A>
        + Allocator<f64, D::LinStateSize, A>
        + Allocator<f64, A, D::LinStateSize>,
{
    pub fn ekf(
        prop: &'a mut Propagator<'a, D, E>,
        kf: &'a mut K,
        devices: &'a [N],
        simultaneous_msr: bool,
        num_expected_msr: usize,
        trigger: T,
    ) -> Self {
        Self {
            prop,
            kf,
            devices: &devices,
            simultaneous_msr,
            estimates: Vec::with_capacity(num_expected_msr),
            residuals: Vec::with_capacity(num_expected_msr),
            ekf_trigger: trigger,
            _marker: PhantomData::<A>,
        }
    }

    pub fn default_ekf(
        prop: &'a mut Propagator<'a, D, E>,
        kf: &'a mut K,
        devices: &'a [N],
        trigger: T,
    ) -> Self {
        Self {
            prop,
            kf,
            devices: &devices,
            simultaneous_msr: false,
            estimates: Vec::with_capacity(10_000),
            residuals: Vec::with_capacity(10_000),
            ekf_trigger: trigger,
            _marker: PhantomData::<A>,
        }
    }

    /// Allows to smooth the provided estimates. Returns an array of smoothed estimates.
    ///
    /// Estimates must be ordered in chronological order. This function will smooth the
    /// estimates from the last in the list to the first one.
    pub fn smooth(&mut self) -> Option<FilterError> {
        debug!("Smoothing {} estimates", self.estimates.len());
        let mut smoothed = Vec::with_capacity(self.estimates.len());

        for estimate in self.estimates.iter().rev() {
            let mut sm_est = estimate.clone();
            // TODO: Ensure that SNC was _not_ enabled
            let mut stm_inv = estimate.stm().clone();
            if !stm_inv.try_inverse_mut() {
                return Some(FilterError::StateTransitionMatrixSingular);
            }
            sm_est.set_covar(&stm_inv * estimate.covar() * &stm_inv.transpose());
            sm_est.set_state(&stm_inv * estimate.state());
            smoothed.push(sm_est);
        }

        // And reverse to maintain order
        smoothed.reverse();
        // And store
        self.estimates = smoothed;

        None
    }

    /// Allows processing all measurements without covariance mapping.
    pub fn process_measurements(&mut self, measurements: &[(Epoch, Msr)]) -> Option<FilterError> {
        info!("Processing {} measurements", measurements.len());

        let mut prev_dt = self.kf.previous_estimate().dt();
        let mut reported = vec![false; 11];
        let num_msrs = measurements.len();

        for (msr_cnt, (next_epoch, real_meas)) in measurements.iter().enumerate() {
            // Propagate the dynamics to the measurement, and then start the filter.
            let delta_time = *next_epoch - prev_dt;
            prev_dt = *next_epoch; // Update the epoch for the next computation
            self.prop.until_time_elapsed(delta_time);
            // Update the STM of the KF
            self.kf.update_stm(self.prop.dynamics.stm());
            let (dt, meas_input) = self.prop.dynamics.to_measurement(&self.prop.state());
            // Get the computed observations
            for device in self.devices.iter() {
                if let Some(computed_meas) = device.measure(&meas_input) {
                    if computed_meas.visible() {
                        self.kf.update_h_tilde(computed_meas.sensitivity());
                        match self.kf.measurement_update(
                            dt,
                            real_meas.observation(),
                            computed_meas.observation(),
                        ) {
                            Ok((est, res)) => {
                                // Switch to extended if necessary, and update the dynamics and such
                                if !self.kf.is_extended() && self.ekf_trigger.enable_ekf(&est) {
                                    self.kf.set_extended(true);
                                    info!("EKF now enabled");
                                }
                                if self.kf.is_extended() {
                                    self.prop.dynamics.set_estimated_state(
                                        self.prop.dynamics.estimated_state() + est.state(),
                                    );
                                }
                                self.estimates.push(est);
                                self.residuals.push(res);
                            }
                            Err(e) => return Some(e),
                        }
                        if !self.simultaneous_msr {
                            break;
                        }
                    }
                }
            }

            let msr_prct = (10.0 * (msr_cnt as f64) / (num_msrs as f64)) as usize;
            if !reported[msr_prct] {
                info!(
                    "[ODProcess] {:>3}% done ({:.0} measurements processed)",
                    10 * msr_prct,
                    msr_cnt
                );
                reported[msr_prct] = true;
            }
        }
        // Always report the 100% mark
        if !reported[10] {
            info!(
                "[ODProcess] {:>3}% done ({:.0} measurements processed)",
                100, num_msrs
            );
        }

        None
    }

    /// Allows processing all measurements with covariance mapping.
    ///
    /// Important notes:
    /// + the measurements have be to mapped to a fixed time corresponding to the step of the propagator
    pub fn process_measurements_covar(
        &mut self,
        prop_rx: &Receiver<D::StateType>,
        measurements: &[(Epoch, Msr)],
    ) -> Option<FilterError> {
        assert!(
            !measurements.is_empty(),
            "must have at least one measurement (for propagation time)"
        );
        // Start by propagating the estimator (on the same thread).
        let num_msrs = measurements.len();
        let prop_time = measurements[num_msrs - 1].0 - self.kf.previous_estimate().dt();
        info!(
            "Propagating for {} seconds (~ {:.3} days)",
            prop_time,
            prop_time / 86_400.0
        );

        self.prop.until_time_elapsed(prop_time);
        info!(
            "Processing {} measurements with covariance mapping",
            num_msrs
        );

        let mut msr_cnt = 0_usize;
        let mut reported = vec![false; 11];

        while let Ok(prop_state) = prop_rx.try_recv() {
            // Get the datetime and info needed to compute the theoretical measurement according to the model
            let (dt, meas_input) = self.prop.dynamics.to_measurement(&prop_state);

            let mut num_msr_processed = 0_u32;
            loop {
                // Update the STM of the KF (needed between each measurement or time update)
                let stm = self.prop.dynamics.extract_stm(&prop_state);
                self.kf.update_stm(stm);
                if msr_cnt < num_msrs {
                    // Get the next measurement
                    let (next_epoch, real_meas) = &measurements[msr_cnt];
                    if *next_epoch < dt {
                        // We missed a measurement! Let's try to catch up.
                        error!("Skipped measurement #{} -- measurement timestamps not in sync with propagator!\n{:?}\n{:?}", msr_cnt, *next_epoch, dt);
                        msr_cnt += 1;
                        continue;
                    } else if *next_epoch > dt {
                        // No measurement can be used here, let's just do a time update (unless we have already done a time update)
                        if num_msr_processed == 0 {
                            debug!("time update {:?}", dt);
                            match self.kf.time_update(dt) {
                                Ok(est) => {
                                    if self.kf.is_extended() {
                                        self.prop.dynamics.set_estimated_state(
                                            self.prop.dynamics.estimated_state() + est.state(),
                                        );
                                    }
                                    self.estimates.push(est);
                                }
                                Err(e) => return Some(e),
                            }
                        }
                        break; // Move on to the next propagator output
                    } else {
                        // The epochs match, so this is a valid measurement to use
                        // Get the computed observations
                        for device in self.devices.iter() {
                            if let Some(computed_meas) = device.measure(&meas_input) {
                                if computed_meas.visible() {
                                    self.kf.update_h_tilde(computed_meas.sensitivity());
                                    match self.kf.measurement_update(
                                        dt,
                                        real_meas.observation(),
                                        computed_meas.observation(),
                                    ) {
                                        Ok((est, res)) => {
                                            debug!(
                                                "msr update msr #{} {}",
                                                msr_cnt,
                                                dt.as_gregorian_utc_str()
                                            );
                                            // Switch to EKF if necessary, and update the dynamics and such
                                            if !self.kf.is_extended()
                                                && self.ekf_trigger.enable_ekf(&est)
                                            {
                                                self.kf.set_extended(true);
                                                info!("EKF now enabled");
                                            }
                                            if self.kf.is_extended() {
                                                self.prop.dynamics.set_estimated_state(
                                                    self.prop
                                                        .dynamics
                                                        .extract_estimated_state(&prop_state)
                                                        + est.state(),
                                                );
                                            }
                                            self.estimates.push(est);
                                            self.residuals.push(res);
                                        }
                                        Err(e) => return Some(e),
                                    }
                                    if !self.simultaneous_msr {
                                        break;
                                    }
                                }
                            }
                        }
                        // And increment the measurement counter
                        msr_cnt += 1;
                        num_msr_processed += 1;
                        let msr_prct = (10.0 * (msr_cnt as f64) / (num_msrs as f64)) as usize;
                        if !reported[msr_prct] {
                            info!(
                                "[ODProcess] {:>3}% done ({:.0} measurements processed)",
                                10 * msr_prct,
                                msr_cnt
                            );
                            reported[msr_prct] = true;
                        }
                    }
                } else {
                    // No more measurements, we can only do a time update
                    debug!("final time update {:?}", dt);
                    match self.kf.time_update(dt) {
                        Ok(est) => {
                            if self.kf.is_extended() {
                                self.prop.dynamics.set_estimated_state(
                                    self.prop.dynamics.extract_estimated_state(&prop_state)
                                        + est.state(),
                                );
                            }
                            self.estimates.push(est);
                        }
                        Err(e) => return Some(e),
                    }
                    // Leave the loop {...} which processes several measurements at once
                    break;
                }
            }
        }

        // Always report the 100% mark
        if !reported[10] {
            info!(
                "[ODProcess] {:>3}% done ({:.0} measurements processed)",
                100, num_msrs
            );
        }

        None
    }

    /// Allows for covariance mapping without processing measurements
    pub fn map_covar(
        &mut self,
        prop_rx: &Receiver<D::StateType>,
        end_epoch: Epoch,
    ) -> Option<FilterError> {
        // Start by propagating the estimator (on the same thread).
        let prop_time = end_epoch - self.kf.previous_estimate().dt();
        info!("Propagating for {} seconds", prop_time);

        self.prop.until_time_elapsed(prop_time);
        info!("Mapping covariance");

        while let Ok(prop_state) = prop_rx.try_recv() {
            // Get the datetime
            let (dt, _) = self.prop.dynamics.to_measurement(&prop_state);

            // Update the STM of the KF (needed between each measurement or time update)
            let stm = self.prop.dynamics.extract_stm(&prop_state);
            self.kf.update_stm(stm);
            info!("final time update {:?}", dt);
            match self.kf.time_update(dt) {
                Ok(est) => {
                    if self.kf.is_extended() {
                        let est_state = est.state().clone();
                        self.prop.dynamics.set_estimated_state(
                            self.prop.dynamics.extract_estimated_state(&prop_state) + est_state,
                        );
                    }
                    self.estimates.push(est);
                }
                Err(e) => return Some(e),
            }
        }

        None
    }
}

impl<
        'a,
        D: Estimable<MsrIn, LinStateSize = M::StateSize>,
        E: ErrorCtrl,
        M: Measurement,
        N: MeasurementDevice<M, MsrIn>,
        A: DimName,
        K: Filter<D::LinStateSize, A, M::MeasurementSize>,
        MsrIn,
    > ODProcess<'a, D, E, M, N, CkfTrigger, A, K, MsrIn>
where
    DefaultAllocator: Allocator<f64, D::StateSize>
        + Allocator<f64, M::MeasurementSize>
        + Allocator<f64, M::MeasurementSize, M::StateSize>
        + Allocator<f64, D::LinStateSize>
        + Allocator<f64, M::MeasurementSize, M::MeasurementSize>
        + Allocator<f64, M::MeasurementSize, D::LinStateSize>
        + Allocator<f64, D::LinStateSize, M::MeasurementSize>
        + Allocator<f64, D::LinStateSize, D::LinStateSize>
        + Allocator<f64, A, A>
        + Allocator<f64, D::LinStateSize, A>
        + Allocator<f64, A, D::LinStateSize>,
{
    pub fn ckf(
        prop: &'a mut Propagator<'a, D, E>,
        kf: &'a mut K,
        devices: &'a [N],
        simultaneous_msr: bool,
        num_expected_msr: usize,
    ) -> Self {
        Self {
            prop,
            kf,
            devices: &devices,
            simultaneous_msr,
            estimates: Vec::with_capacity(num_expected_msr),
            residuals: Vec::with_capacity(num_expected_msr),
            ekf_trigger: CkfTrigger {},
            _marker: PhantomData::<A>,
        }
    }

    pub fn default_ckf(
        prop: &'a mut Propagator<'a, D, E>,
        kf: &'a mut K,
        devices: &'a [N],
    ) -> Self {
        Self {
            prop,
            kf,
            devices: &devices,
            simultaneous_msr: false,
            estimates: Vec::with_capacity(10_000),
            residuals: Vec::with_capacity(10_000),
            ekf_trigger: CkfTrigger {},
            _marker: PhantomData::<A>,
        }
    }
}
/// A trait detailing when to switch to from a CKF to an EKF
pub trait EkfTrigger {
    fn enable_ekf<S, E>(&mut self, est: &E) -> bool
    where
        S: DimName,
        E: Estimate<S>,
        DefaultAllocator: Allocator<f64, S> + Allocator<f64, S, S>;
}

/// CkfTrigger will never switch a KF to an EKF
pub struct CkfTrigger;

impl EkfTrigger for CkfTrigger {
    fn enable_ekf<S, E>(&mut self, _est: &E) -> bool
    where
        S: DimName,
        E: Estimate<S>,
        DefaultAllocator: Allocator<f64, S> + Allocator<f64, S, S>,
    {
        false
    }
}

/// An EkfTrigger on the number of measurements processed
pub struct NumMsrEkfTrigger {
    pub num_msrs: usize,
    cur_msrs: usize,
}

impl NumMsrEkfTrigger {
    pub fn init(num_msrs: usize) -> Self {
        Self {
            num_msrs,
            cur_msrs: 0,
        }
    }
}

impl EkfTrigger for NumMsrEkfTrigger {
    fn enable_ekf<S, E>(&mut self, _est: &E) -> bool
    where
        S: DimName,
        E: Estimate<S>,
        DefaultAllocator: Allocator<f64, S> + Allocator<f64, S, S>,
    {
        self.cur_msrs += 1;
        self.cur_msrs >= self.num_msrs
    }
}
