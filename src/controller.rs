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

const DEFAULT_MIN_TIMEOUT: u64 = 10; // scenario minimal timeout in seconds
const DEFAULT_CALL_TIMEOUT: u64 = 1000; // call_sync 1s default timeout
const DEFAULT_CALL_DELAY: u64 = 100; // call_sync 100ms delay
const DEFAULT_DELAY_PERCENT: u64 = 10; // reduce delay by 10
const DEFAULT_DELAY_MIN: u64 = 50; // reduce delay by 10
const DEFAULT_DELAY_MAX: u64 = 100; // reduce delay by 10

#[derive(Clone, Copy)]
pub struct InjectorDelayConf {
    pub percent: u64,
    pub min: u64,
    pub max: u64,
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

    pub fn get_duration(&self, millis: u64) -> time::Duration {
        let delay = millis * self.percent / 100;
        let delay = if delay > self.max {
            self.max
        } else if delay < self.min {
            self.min
        } else {
            delay
        };
        time::Duration::from_millis(delay)
    }
}

#[derive(Clone, Copy)]
pub struct InjectorRetryConf {
    pub delay: time::Duration,
    pub timeout: time::Duration,
    pub count: u32,
}

impl InjectorRetryConf {
    pub fn default() -> Self {
        Self {
            delay: time::Duration::from_millis(DEFAULT_CALL_DELAY),
            timeout: time::Duration::from_millis(DEFAULT_CALL_TIMEOUT),
            count: 1,
        }
    }
    pub fn from_jsonc(jsonc: JsoncObj, delay_conf: &InjectorDelayConf) -> Result<Self, AfbError> {
        Ok(Self {
            delay: delay_conf.get_duration(jsonc.default("delay", DEFAULT_CALL_TIMEOUT)?),
            timeout: time::Duration::from_millis(jsonc.default("timeout", DEFAULT_CALL_TIMEOUT)?),
            count: jsonc.default("count", 1)?,
        })
    }
}

fn spawn_one_transaction(
    api: AfbApiV4,
    transac: &mut InjectorEntry,
    event: Option<&AfbEvent>,
) -> Result<(), AfbError> {
    // send result as event
    let jreply = JsoncObj::new();
    jreply.add("uid", transac.uid)?;
    jreply.add("verb", transac.verb)?;

    // initial request delay
    thread::sleep(transac.delay);

    for idx in 0..transac.retry.count {
        transac.status = SimulationStatus::Pending;
        transac.status = match injector_launch_transac(api, transac) {
            Ok(value) => value,
            Err(error) => {
                // api/verb did not return
                if idx < transac.retry.count {
                    jreply.add("status", "SimulationStatus::Retry")?;
                    match event {
                        Some(evt) => {
                            evt.push(jreply.clone());
                        }
                        None => {
                            println!(
                                "--[{}:{}] SimulationStatus::Retry {}",
                                transac.uid, idx, jreply
                            );
                        }
                    }
                    thread::sleep(transac.retry.delay);
                    SimulationStatus::Retry
                } else {
                    jreply.add("error", error.to_jsonc()?)?;
                    match event {
                        Some(evt) => {
                            evt.push(jreply.clone());
                        }
                        None => {
                            println!(
                                "--[{}:{}] SimulationStatus::Retry {}",
                                transac.uid, idx, jreply
                            );
                        }
                    }
                    return afb_error!(transac.uid, "call_async fail {}", error);
                }
            }
        };
        match &transac.status {
            SimulationStatus::Done | SimulationStatus::Check => break,
            SimulationStatus::Fail(error) => {
                // api/verb return invalid values
                jreply.add("error", error.to_jsonc()?)?;
                match event {
                    Some(evt) => {
                        evt.push(jreply.clone());
                    }
                    None => {
                        println!("--[{}] SimulationStatus::Fail {}", transac.uid, jreply);
                    }
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

    let status = format!("{:?}", &transac.status);
    jreply.add("status", &status)?;
    match event {
        Some(evt) => {
            evt.push(jreply.clone());
        }
        None => {
            println!("--[{}] {} {}", transac.uid, &status, jreply);
        }
    }
    Ok(())
}

pub struct JobScenarioParam {
    pub api: AfbApiV4,
    pub injector: &'static Injector,
    pub event: Option<&'static AfbEvent>,
}

pub fn job_scenario_exec(param: &JobScenarioParam) -> Result<(), AfbError> {
    // loop on scenario transactions
    for idx in 0..param.injector.count {
        let mut state = param.injector.lock_state()?;
        let transac = &mut state.entries[idx];
        spawn_one_transaction(param.api, transac, param.event)?;
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

pub struct InjectorEntry {
    pub uid: &'static str,
    pub target: &'static str,
    pub verb: &'static str,
    pub queries: JsoncObj,
    pub expects: JsoncObj,
    pub status: SimulationStatus,
    pub retry: InjectorRetryConf,
    pub delay: time::Duration,
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
        scenario_timeout: u64,
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

            let delay = transac.default("delay", DEFAULT_CALL_DELAY)?;

            let retry_conf = match transac.optional::<JsoncObj>("retry")? {
                None => retry_conf,
                Some(jretry) => InjectorRetryConf::from_jsonc(jretry, &delay_conf)?,
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
                retry: retry_conf,
                delay: delay_conf.get_duration(delay),
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
                        "ok {:04} - {}({}/{})  # Checked",
                        idx, transac.verb, transac.uid, transac.retry.count
                    )
                }
                SimulationStatus::Fail(error) => {
                    format!(
                        "fx {:04} - {}({}/{})  # Fail {}",
                        idx, transac.verb, transac.uid, error, transac.retry.count
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
    pub uid: &'static str,
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
