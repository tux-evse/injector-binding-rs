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

use afbv4::prelude::*;
use std::cell::Cell;
use std::sync::{Mutex, MutexGuard};

pub type InjectorJobPostCb =
    fn(api: AfbApiV4, transac: &mut InjectorEntry) -> Result<SimulationStatus, AfbError>;

pub struct JobTransactionContext {
    event: &'static AfbEvent,
    callback: InjectorJobPostCb,
    running: bool,
    retry_conf: InjectorRetryConf,
}

pub struct JobTransactionParam<'a> {
    api: AfbApiV4,
    idx: usize,
    state: MutexGuard<'a, ScenarioState>,
}

#[derive(Clone, Copy)]
pub struct InjectorRetryConf {
    pub delay: i32,
    pub timeout: i32,
    pub count: i32,
}

fn job_transaction_cb(
    _job: &AfbSchedJob,
    signal: i32,
    jobctx: &AfbCtxData,
    context: &AfbCtxData,
) -> Result<(), AfbError> {
    let param = jobctx.get_mut::<JobTransactionParam>()?;
    let ctx = context.get_mut::<JobTransactionContext>()?;

    let mut transac = &mut param.state.entries[param.idx];

    if signal != 0 {
        let error = AfbError::new(
            "job-transaction-cb",
            -62,
            format!("{} timeout", transac.uid),
        );

        // send result as event
        let jreply = JsoncObj::new();
        jreply.add("uid", transac.uid)?;
        jreply.add(
            "status",
            format!("{:?}", SimulationStatus::Timeout).as_str(),
        )?;
        ctx.event.push(jreply);

        transac.status = SimulationStatus::Fail(error);
        jobctx.free::<JobTransactionParam>();

        return afb_error!(
            "job-transaction-cb",
            "uid:{} transaction killed",
            transac.uid
        );
    }

    if ctx.running {
        for idx in 0..ctx.retry_conf.count {
            transac.status = SimulationStatus::Pending;
            transac.status = (ctx.callback)(param.api, &mut transac)?;
            match &transac.status {
                SimulationStatus::Done | SimulationStatus::Check => break,
                SimulationStatus::Fail(_error) => {}
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
        ctx.running = false;
    } else {
        transac.status = SimulationStatus::Ignored;
    };

    let response = if let SimulationStatus::Fail(error) = &transac.status {
        afb_error!(
            "job_transaction_cb",
            "unexpected status for uid:{} count:{} error:{}",
            transac.uid,
            ctx.retry_conf.count,
            error
        )
    } else {
        Ok(())
    };

    // send result as event
    let jreply = JsoncObj::new();
    jreply.add("uid", transac.uid)?;
    jreply.add("verb", transac.verb)?;
    jreply.add("status", format!("{:?}", transac.status).as_str())?;
    ctx.event.push(jreply);
    jobctx.free::<JobTransactionParam>();
    response
}

pub struct JobScenarioContext {
    callback: InjectorJobPostCb,
    retry_conf: InjectorRetryConf,
    count: usize,
}

pub struct JobScenarioParam {
    api: AfbApiV4,
    injector: &'static Injector,
    event: &'static AfbEvent,
}

fn job_scenario_cb(
    _job: &AfbSchedJob,
    signal: i32,
    params: &AfbCtxData,
    context: &AfbCtxData,
) -> Result<(), AfbError> {
    let param = params.get_ref::<JobScenarioParam>()?;
    let ctx = context.get_ref::<JobScenarioContext>()?;

    // job was kill from API
    if signal != 0 {
        context.free::<JobScenarioContext>();
        return Ok(());
    }

    // compute global timeout in case count > 1
    let timeout = ctx.retry_conf.timeout * ctx.retry_conf.count;

    let job = AfbSchedJob::new("iso15118-transac")
        .set_exec_watchdog(timeout)
        .set_group(1)
        .set_callback(job_transaction_cb)
        .set_context(JobTransactionContext {
            event: param.event,
            callback: ctx.callback,
            running: true,
            retry_conf: ctx.retry_conf,
        })
        .finalize();

    for idx in 0..ctx.count {
        job.post(
            0,
            JobTransactionParam {
                idx,
                state: param.injector.lock_state()?,
                api: param.api,
            },
        )?;
    }

    job.terminate();
    Ok(())
}

#[derive(Debug)]
pub enum SimulationStatus {
    Pending,
    Done,
    Check,
    Ignored,
    Idle,
    Timeout,
    Fail(AfbError),
}

pub struct InjectorEntry {
    pub uid: &'static str,
    pub target: &'static str,
    pub verb: &'static str,
    pub queries: JsoncObj,
    pub expects: JsoncObj,
    pub status: SimulationStatus,
    pub sequence: usize,
}

pub struct ScenarioState {
    pub entries: Vec<InjectorEntry>,
}

pub struct Injector {
    uid: &'static str,
    job: &'static AfbSchedJob,
    count: usize,
    data_set: Mutex<ScenarioState>,
}

impl Injector {
    pub fn new(
        uid: &'static str,
        target: Option<&'static str>,
        protocol: &'static str,
        config: JsoncObj,
        retry_conf: InjectorRetryConf,
        callback: InjectorJobPostCb,
    ) -> Result<&'static Self, AfbError> {
        let mut data_set = ScenarioState {
            entries: Vec::new(),
        };

        let target = match target {
            Some(value) => value,
            None => return afb_error!("injector-new", "missing target api from config"),
        };

        for idx in 0..config.count()? {
            let transac = config.index::<JsoncObj>(idx)?;
            let uid = transac.get::<&str>("uid")?;
            let queries = JsoncObj::array();
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
                    let name = format!("{}:{}_req", protocol, uid.replace("-", "_"));
                    to_static_str(name)
                }
            };

            data_set.entries.push(InjectorEntry {
                uid,
                verb,
                queries: queries,
                expects: expects,
                status: SimulationStatus::Idle,
                sequence: 0,
                target,
            });
        }

        let job = AfbSchedJob::new("iso-15118-Injector")
            .set_callback(job_scenario_cb)
            .set_context(JobScenarioContext {
                retry_conf,
                callback,
                count: data_set.entries.len(),
            });

        let this = Self {
            uid,
            job,
            count: config.count()?,
            data_set: Mutex::new(data_set),
        };

        Ok(Box::leak(Box::new(this)))
    }

    #[track_caller]
    pub fn lock_state(&self) -> Result<MutexGuard<'_, ScenarioState>, AfbError> {
        let guard = self.data_set.lock().unwrap();
        Ok(guard)
    }

    pub fn start(
        &'static self,
        afb_rqt: &AfbRequest,
        event: &'static AfbEvent,
    ) -> Result<i32, AfbError> {
        let api = afb_rqt.get_apiv4();
        let job_id = self.job.post(
            1,
            JobScenarioParam {
                injector: self,
                event,
                api,
            },
        )?;

        Ok(job_id)
    }

    pub fn stop(&self, job_id: i32) -> Result<JsoncObj, AfbError> {
        self.job.abort(job_id)?;
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
                    format!("ok {} - {}  # Response uncheck", idx, transac.uid)
                }
                SimulationStatus::Check => {
                    format!("ok {} - {}  # Response checked", idx, transac.uid)
                }
                SimulationStatus::Fail(error) => {
                    format!("fx {} - {}  # {}", idx, transac.uid, error)
                }
                _ => format!("fx {} - {}  # {:?}", idx, transac.uid, transac.status),
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
}

impl Responder {
    pub fn new() -> &'static Self {
        let this = Responder {
            nonce: Cell::new(0),
        };
        Box::leak(Box::new(this))
    }

    pub fn reset(&'static self) {
        self.nonce.set(self.nonce.get() + 1);
    }

    pub fn get_nonce(&'static self) -> u32 {
        self.nonce.get()
    }
}
