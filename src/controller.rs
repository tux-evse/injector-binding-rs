/*
 * Copyright (C) 2015-2022 IoT.bzh Company
 * Author: Fulup Ar Foll <fulup@iot.bzh>
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 */

use crate::prelude::*;
use afbv4::prelude::*;
use std::cell::Cell;
use std::sync::{Mutex, MutexGuard};
use std::{thread, time};

const DEFAULT_MIN_TIMEOUT: u32 = 10; // scenario minimal timeout in seconds
const DEFAULT_CALL_TIMEOUT: u32 = 1000; // call_sync 1s default timeout
const DEFAULT_CALL_DELAY: u32 = 100; // call_sync 100ms delay
const DEFAULT_DELAY_PERCENT: u32 = 10; // reduce delay by 10
const DEFAULT_DELAY_MIN: u32 = 50; // reduce delay by 10
const DEFAULT_DELAY_MAX: u32 = 100; // reduce delay by 10

#[derive(Clone, Copy)]
pub struct InjectorDelayConf {
    pub percent: u32,
    pub min: u32,
    pub max: u32,
}

impl InjectorDelayConf {
    pub fn default() -> Self {
        Self {
            percent: DEFAULT_DELAY_PERCENT,
            min: DEFAULT_DELAY_MIN,
            max: DEFAULT_DELAY_MAX,
        }
    }
    pub fn from_jsonc(jsonc: JsoncObj) -> Result<Self, AfbError> {
        Ok(Self {
            percent: jsonc.default("percent", DEFAULT_DELAY_PERCENT)?,
            min: jsonc.default("min", DEFAULT_DELAY_MIN)?,
            max: jsonc.default("max", DEFAULT_DELAY_MAX)?,
        })
    }

    pub fn get_duration(&self, millis: u32) -> time::Duration {
        let delay = millis * self.percent / 100;
        let delay = if delay > self.max {
            self.max
        } else if delay < self.min {
            self.min
        } else {
            delay
        };
        time::Duration::from_millis(delay as u64)
    }
}

#[derive(Clone, Copy)]
pub struct InjectorRetryConf {
    pub delay: time::Duration,
    pub timeout: u32,
    pub count: u32,
}

impl InjectorRetryConf {
    pub fn default() -> Self {
        Self {
            delay: time::Duration::from_millis(100),
            timeout: DEFAULT_CALL_TIMEOUT,
            count: 1,
        }
    }
    pub fn from_jsonc(jsonc: JsoncObj) -> Result<Self, AfbError> {
        Ok(Self {
            delay: time::Duration::from_millis(jsonc.default("delay", 100)?),
            timeout: jsonc.default("timeout", DEFAULT_CALL_TIMEOUT)?,
            count: jsonc.default("count", 1)?,
        })
    }
}

fn spawn_one_transaction(
    api: AfbApiV4,
    transac: &mut InjectorEntry,
    watchdog: &AfbSchedJob,
    event: Option<&AfbEvent>,
) -> Result<(), AfbError> {
    // send result as event
    let jreply = JsoncObj::new();
    jreply.add("uid", transac.uid)?;
    jreply.add("verb", transac.verb)?;

    for idx in 0..transac.retry.count {
        transac.status = SimulationStatus::Pending;
        thread::sleep(transac.retry.delay); // force delay between transaction
        transac.status = match injector_jobpost_transac(api, watchdog, transac) {
            Ok(value) => value,
            Err(error) => {
                // api/verb did not return
                if idx < transac.retry.count {
                    jreply.add("status", "SimulationStatus::Retry")?;
                    if let Some(evt) = event {
                        evt.push(jreply.clone());
                    }
                    SimulationStatus::Retry
                } else {
                    jreply.add("error", error.to_jsonc()?)?;
                    if let Some(evt) = event {
                        evt.push(jreply.clone());
                    }
                    return afb_error!("job_transaction_cb", "callsync fail {}", error);
                }
            }
        };
        match &transac.status {
            SimulationStatus::Done | SimulationStatus::Check => break,
            SimulationStatus::Fail(error) => {
                // api/verb return invalid values
                jreply.add("error", error.to_jsonc()?)?;
                if let Some(evt) = event {
                    evt.push(jreply.clone());
                }
                break;
            }
            SimulationStatus::Retry => {}
            _ => {
                return afb_error!(
                    "job_transaction_cb",
                    "unexpected status for uid:{} count:{}",
                    transac.uid,
                    idx
                )
            }
        }
    }

    if let SimulationStatus::Fail(error) = &transac.status {
        return afb_error!(
            "job_transaction_cb",
            "unexpected status for uid:{} count:{} error:{}",
            transac.uid,
            transac.retry.count,
            error
        );
    }

    jreply.add("status", format!("{:?}", &transac.status).as_str())?;
    if let Some(evt) = event {
        evt.push(jreply.clone());
    }
    Ok(())
}

pub struct JobScenarioParam {
    pub api: AfbApiV4,
    pub injector: &'static Injector,
    pub event: Option<&'static AfbEvent>,
}

pub fn job_scenario_exec(param: &JobScenarioParam) -> Result<(), AfbError> {
    // per transaction job
    let timeout_job = AfbSchedJob::new("iso15118-transac")
        .set_group(1)
        .set_callback(injector_async_timeout)
        .finalize();

    // loop on scenario transactions
    for idx in 0..param.injector.count {
        let mut state = param.injector.lock_state()?;
        let transac = &mut state.entries[idx];

        spawn_one_transaction(param.api, transac, timeout_job, param.event)?;
    }
    Ok(())
}

fn job_scenario_cb(
    _job: &AfbSchedJob,
    signal: i32,
    params: &AfbCtxData,
    _context: &AfbCtxData,
) -> Result<(), AfbError> {
    let param = params.get_ref::<JobScenarioParam>()?;

    // job was kill from API
    if signal != 0 {
        return Ok(());
    }

    job_scenario_exec(param)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub enum SimulationStatus {
    Pending,
    Done,
    Check,
    InvalidSequence,
    Skip,
    Timeout,
    Retry,
    Fail(AfbError),
}

impl SimulationStatus {
    pub fn is_pending(&self) -> bool {
        match self {
            SimulationStatus::Pending => true,
            _ => false,
        }
    }
}

pub struct InjectorEntry {
    pub uid: &'static str,
    pub target: &'static str,
    pub verb: &'static str,
    pub queries: JsoncObj,
    pub expects: JsoncObj,
    pub status: SimulationStatus,
    pub delay: time::Duration,
    pub retry: InjectorRetryConf,
    pub sequence: usize,
}

pub struct ScenarioState {
    pub entries: Vec<InjectorEntry>,
}

pub struct Injector {
    uid: &'static str,
    scenario_job: &'static AfbSchedJob,
    count: usize,
    data_set: Mutex<ScenarioState>,
}

impl Injector {
    pub fn new(
        uid: &'static str,
        target: Option<&'static str>,
        prefix: &'static str,
        scenario_timeout: u32,
        transactions: JsoncObj,
        delay_conf: InjectorDelayConf,
        retry_conf: InjectorRetryConf,
    ) -> Result<&'static Self, AfbError> {
        let mut data_set = ScenarioState {
            entries: Vec::new(),
        };

        let target = match target {
            Some(value) => value,
            None => return afb_error!("injector-new", "missing target api from transactions"),
        };

        // reduce timeout depending on delay percentage ration
        let mut scenario_timeout = scenario_timeout * delay_conf.percent / 100;
        if scenario_timeout < DEFAULT_MIN_TIMEOUT {
            scenario_timeout = DEFAULT_MIN_TIMEOUT;
        }

        for idx in 0..transactions.count()? {
            let transac = transactions.index::<JsoncObj>(idx)?;
            let uid = transac.get::<&str>("uid")?;
            let queries = JsoncObj::array();
            let delay = delay_conf.get_duration(transac.default("delay", DEFAULT_CALL_DELAY)?);
            let retry_conf = match transac.optional::<JsoncObj>("retry")? {
                None => retry_conf,
                Some(jretry) => InjectorRetryConf::from_jsonc(jretry)?,
            };
            if let Some(value) = transac.optional::<JsoncObj>("query")? {
                queries.append(value)?;
            }
            let expects = JsoncObj::array();
            if let Some(value) = transac.optional::<JsoncObj>("expect")? {
                expects.append(value)?;
            }
            let verb = match transac.optional::<&'static str>("verb")? {
                Some(value) => value,
                None => {
                    let name = format!("{}:{}_req", prefix, uid.replace("-", "_"));
                    to_static_str(name)
                }
            };

            data_set.entries.push(InjectorEntry {
                uid,
                verb,
                queries,
                expects,
                status: SimulationStatus::Skip,
                delay,
                retry: retry_conf,
                sequence: 0,
                target,
            });
        }

        let scenario_job = AfbSchedJob::new("iso-15118-Injector")
            .set_callback(job_scenario_cb)
            .set_exec_watchdog(scenario_timeout as i32);

        let this = Self {
            uid,
            scenario_job,
            count: transactions.count()?,
            data_set: Mutex::new(data_set),
        };

        Ok(Box::leak(Box::new(this)))
    }

    pub fn get_uid(&self) -> &str {
        self.uid
    }

    #[track_caller]
    pub fn lock_state(&self) -> Result<MutexGuard<'_, ScenarioState>, AfbError> {
        let guard = self.data_set.lock().unwrap();
        Ok(guard)
    }

    pub fn post_scenario(
        &'static self,
        api: AfbApiV4,
        event: &'static AfbEvent,
    ) -> Result<i32, AfbError> {
        let job_id = self.scenario_job.post(
            100, // 100ms start delay
            JobScenarioParam {
                injector: self,
                event: Some(event),
                api,
            },
        )?;

        Ok(job_id)
    }

    pub fn kill_scenario(&self, job_id: i32) -> Result<JsoncObj, AfbError> {
        self.scenario_job.abort(job_id)?;
        self.get_result()
    }

    pub fn get_result(&self) -> Result<JsoncObj, AfbError> {
        let state = self.lock_state()?;
        let result = JsoncObj::array();
        result.append(format!("1..{} # {}", self.count, self.uid).as_str())?;
        for idx in 0..self.count {
            let transac = &state.entries[idx];
            let status = match &transac.status {
                SimulationStatus::Done => {
                    format!("ok {:04} - {}({})  # Done", idx, transac.verb, transac.uid)
                }
                SimulationStatus::Check => {
                    format!(
                        "ok {:04} - {}({})  # Checked",
                        idx, transac.verb, transac.uid
                    )
                }
                SimulationStatus::Fail(error) => {
                    format!(
                        "fx {:04} - {}({})  # Fail {}",
                        idx, transac.verb, transac.uid, error
                    )
                }
                _ => format!(
                    "fx {:04} - {}({}) # Misc {:?}",
                    idx, transac.verb, transac.uid, transac.status
                ),
            };
            result.append(status.as_str())?;
        }
        Ok(result)
    }
}

pub struct ResponderEntry {
    pub queries: JsoncObj,
    pub expects: JsoncObj,
    pub sequence: usize,
    pub nonce: u32,
    pub responder: &'static Responder,
}

pub struct ResponderReset {
    pub responder: &'static Responder,
}

pub struct Responder {
    nonce: Cell<u32>,
    loop_reset: bool,
}

impl Responder {
    pub fn new(loop_reset: bool) -> &'static Self {
        let this = Responder {
            nonce: Cell::new(0),
            loop_reset,
        };
        Box::leak(Box::new(this))
    }

    pub fn reset(&self) {
        self.nonce.set(self.nonce.get() + 1);
    }

    pub fn get_nonce(&self) -> u32 {
        self.nonce.get()
    }

    pub fn get_loop(&self) -> bool {
        self.loop_reset
    }
}
