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
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::{env, time};

AfbDataConverter!(scenario_actions, ScenarioAction);
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase", tag = "action")]
pub enum ScenarioAction {
    #[default]
    START,
    STOP,
    EXEC,
    RESULT,
}

fn transaction_get_verb(jsonc: &JsoncObj) -> Result<&'static str, AfbError> {
    let uid = jsonc.get::<&'static str>("uid")?;
    let verb = match jsonc.optional::<&'static str>("verb")? {
        Some(value) => value,
        None => {
            let name = format!("{}_req", uid.replace("-", "_"));
            to_static_str(name)
        }
    };
    Ok(verb)
}

// sort scenario to handle duplicate verbs with different arguments
extern "C" fn scenario_sort_cb(
    jso1: *const ::std::os::raw::c_void,
    jso2: *const ::std::os::raw::c_void,
) -> i32 {
    // json sort callback provide void address of jso pointer
    let jsonc1 = JsoncObj::import(jso1 as *const *const JsoncJso).unwrap();
    let jsonc2 = JsoncObj::import(jso2 as *const *const JsoncJso).unwrap();

    let verb1 = match transaction_get_verb(&jsonc1) {
        Err(_) => return -1,
        Ok(value) => value,
    };

    let verb2 = match transaction_get_verb(&jsonc2) {
        Err(_) => return -1,
        Ok(value) => value,
    };

    if verb1.cmp(&verb2) == Ordering::Greater {
        1
    } else {
        0
    }
}

pub struct ScenarioReqCtx {
    _uid: &'static str,
    evt: &'static AfbEvent,
    job_id: i32,
    injector: &'static Injector,
}

fn scenario_action_cb(
    afb_rqt: &AfbRequest,
    args: &AfbRqtData,
    ctx: &AfbCtxData,
) -> Result<(), AfbError> {
    let api = afb_rqt.get_apiv4();
    let ctx = ctx.get_mut::<ScenarioReqCtx>()?;
    let action = args.get::<&ScenarioAction>(0)?;

    match action {
        ScenarioAction::START => {
            ctx.evt.subscribe(afb_rqt)?;
            ctx.job_id = ctx.injector.post_scenario(api, ctx.evt)?;
            afb_rqt.reply(ctx.job_id, 0);
        }

        ScenarioAction::STOP => {
            ctx.evt.unsubscribe(afb_rqt)?;
            let result = ctx.injector.kill_scenario(ctx.job_id)?;
            afb_rqt.reply(result, 0);
            ctx.job_id = 0;
        }

        ScenarioAction::RESULT => {
            let result = ctx.injector.get_result()?;
            afb_rqt.reply(result, 0);
        }

        ScenarioAction::EXEC => {
            ctx.evt.subscribe(afb_rqt)?;
            let param = JobScenarioParam {
                injector: ctx.injector,
                event: Some(ctx.evt),
                api,
            };
            job_scenario_exec(&param)?;
            let result = ctx.injector.get_result()?;
            afb_rqt.reply(result, 0);
        }
    }
    Ok(())
}

pub type Watchdog = Arc<(Mutex<SimulationStatus>, Condvar)>;

struct InjectorAsyncCtx {
    #[allow(dead_code)]
    uid: &'static str,
    expects: JsoncObj,
    semaphore: Watchdog,
}

fn injector_async_response(
    _api: &AfbApi,
    args: &AfbRqtData,
    context: &AfbCtxData,
) -> Result<(), AfbError> {
    let ctx = context.get_ref::<InjectorAsyncCtx>()?;

    let status = match ctx.expects.count()? {
        1 => {
            // injector only use 1st expect element
            if args.get_count() < 1 {
                return afb_error!(
                    "injector-response-cb",
                    "(hoops) response expected, did not yet any"
                );
            }
            let jreceived = args.get::<JsoncObj>(0)?;
            let jexpected = ctx.expects.index::<JsoncObj>(0)?;

            match jreceived.equal(ctx.uid, jexpected.clone(), Jequal::Partial) {
                Ok(_) => SimulationStatus::Check,
                Err(error) => {
                    afb_log_msg!(Error, _api, "received: {}", jreceived);
                    afb_log_msg!(Error, _api, "expected: {}", jexpected);
                    SimulationStatus::Fail(error)
                }
            }
        }
        0 => SimulationStatus::Done,
        _ => {
            return afb_error!(
                "injector-response-cb",
                "(hoops) injection scenario with multiple expect return element"
            )
        }
    };

    let (lock, cvar) = &*ctx.semaphore;
    match lock.lock() {
        Ok(mut value) => {
            *value = status;
            cvar.notify_one();
        }
        Err(_) => {
            return afb_error!(
                "injector-response-cb",
                "(hoops) fail to acquire status semaphore"
            )
        }
    }
    context.free::<InjectorAsyncCtx>();
    Ok(())
}

fn injector_async_request(
    api: AfbApiV4,
    transac: &InjectorEntry,
    semaphore: Watchdog,
) -> Result<(), AfbError> {
    let mut query = AfbParams::new();
    for idx in 0..transac.queries.count()? {
        let jsonc = transac.queries.index::<JsoncObj>(idx)?;
        query.push(jsonc.clone())?;
    }

    let subcall_ctx = InjectorAsyncCtx {
        uid: transac.uid,
        expects: transac.expects.clone(),
        semaphore,
    };

    AfbSubCall::call_async(
        api,
        transac.target,
        transac.verb,
        query,
        injector_async_response,
        subcall_ctx,
    )?;
    Ok(())
}

// call by jobpost when injector run a scenario
pub fn injector_launch_transac(
    api: AfbApiV4,
    transac: &InjectorEntry,
) -> Result<SimulationStatus, AfbError> {
    // create a smephare with condition variable to wait either timeout either async response
    let semaphore = Arc::new((Mutex::new(SimulationStatus::Pending), Condvar::new()));
    //afb_log_msg!(Debug, api, "spawning {}/{}&{}", transac.target, transac.verb, transac.queries);

    // start asynchronous subcall request
    injector_async_request(api, transac, semaphore.clone())?;

    // wait util call return or timeout burn
    let (lock, cvar) = &*semaphore;
    let status = match lock.lock() {
        Ok(value) => {
            //value = cvar.wait(value).unwrap();
            let result = cvar.wait_timeout(value, transac.retry.timeout).unwrap();
            if result.1.timed_out() {
                SimulationStatus::Timeout
            } else {
                result.0.clone()
            }
        }
        Err(_) => SimulationStatus::InvalidSequence,
    };

    Ok(status)
}

// call when activating manually a specific scenario command
fn injector_req_cb(
    afb_rqt: &AfbRequest,
    args: &AfbRqtData,
    ctx: &AfbCtxData,
) -> Result<(), AfbError> {
    let transac = ctx.get_mut::<InjectorEntry>()?;
    let query = args.get::<JsoncObj>(0)?;

    let response = AfbSubCall::call_sync(afb_rqt, transac.target, transac.verb, query)?;
    let argument = response.get::<JsoncObj>(0)?;
    afb_rqt.reply(argument, 0);
    Ok(())
}

// in responding mode send back by iteration count expected result
fn responder_req_cb(
    afb_rqt: &AfbRequest,
    args: &AfbRqtData,
    ctx: &AfbCtxData,
) -> Result<(), AfbError> {
    let transac = ctx.get_mut::<ResponderEntry>()?;

    if transac.nonce != transac.responder.get_nonce() {
        transac.nonce = transac.responder.get_nonce();
        transac.sequence = 0;
    }

    if transac.queries.count()? == transac.sequence {
        if transac.responder.get_loop() {
            transac.sequence = 0
        } else {
            return afb_error!(
                "responder-req-cb",
                "invalid sequence expected:{} got:{}",
                transac.sequence+1,
                transac.queries.count()?
            );
        }
    };

    let received_query = args.get::<JsoncObj>(0)?;
    let expected_query = transac.queries.index::<JsoncObj>(transac.sequence)?;

    match received_query.equal(transac.uid, expected_query.clone(), Jequal::Partial) {
        Ok(_) => {
            let expect = transac.expects.index::<JsoncObj>(transac.sequence)?;
            if expect.len()? == 0 {
                afb_rqt.reply(AFB_NO_DATA, 0);
            } else {
                afb_rqt.reply(expect, 0);
            }
        }

        error => {
            println!(
                "*** \n -- rec:{} \n-- exp:{}",
                received_query, expected_query
            );
            return afb_error!(
                "responder-req-fail",
                "query check return invalid value:{:?}",
                error
            );
        }
    }
    // next run should match with next sequence transaction
    transac.sequence += 1;
    Ok(())
}

#[derive(Clone, Copy)]
enum TransactionVerbCtx {
    Responder(&'static Responder),
    Injector(&'static Injector),
}

fn create_transaction_verb(
    group: &mut AfbGroup,
    verb: &'static str,
    infos: JsoncObj,
    queries: JsoncObj,
    expects: JsoncObj,
    callback: RqtCallback,
    context: TransactionVerbCtx,
    target: Option<&'static str>,
) -> Result<(), AfbError> {
    let transaction_verb = AfbVerb::new(verb)
        .set_info(to_static_str(infos.to_string()))
        .set_callback(callback);

    match context {
        TransactionVerbCtx::Responder(responder) => {
            let context = ResponderEntry {
                uid: verb,
                queries: queries.clone(),
                expects,
                sequence: 0,
                nonce: 0,
                responder,
            };
            transaction_verb.set_context(context);
        }
        TransactionVerbCtx::Injector(_) => {
            let target_api = match target {
                None => return afb_error!("injector-create-verb", "config target api missing"),
                Some(value) => value,
            };

            let context = InjectorEntry {
                queries: queries.clone(),
                expects,
                sequence: 0,
                status: SimulationStatus::InvalidSequence,
                target: target_api,
                uid: verb,
                verb: verb,
                delay: time::Duration::new(0, 0),
                retry: InjectorRetryConf::default(),
            };
            transaction_verb.set_context(context);
        }
    }

    for idx in 0..queries.count()? {
        let info = infos.index::<&str>(idx)?;
        let jsonc = JsoncObj::new();
        jsonc.add("info", info)?;
        for entry in queries.index::<JsoncObj>(idx)?.expand()? {
            jsonc.add(&entry.key, entry.obj)?;
        }
        transaction_verb.add_sample(jsonc)?;
    }
    group.add_verb(transaction_verb.finalize()?);
    Ok(())
}

fn create_transaction_group(
    transactions: JsoncObj,
    uid_scenario: &'static str,
    name_scenario: &'static str,
    callback: RqtCallback,
    context: TransactionVerbCtx,
    target: Option<&'static str>,
) -> Result<&'static AfbGroup, AfbError> {
    let scenario_group = AfbGroup::new(uid_scenario)
        .set_separator(":")
        .set_prefix(name_scenario);

    // sort jsonc transaction by uid/verb to process duplicate verbs
    transactions.sort(Some(scenario_sort_cb))?;

    let mut previous_verb = "";
    let mut infos = JsoncObj::array();
    let mut queries = JsoncObj::array();
    let mut expects = JsoncObj::array();

    for idx in 0..transactions.count()? {
        // extract data from transaction
        let transac = transactions.index::<JsoncObj>(idx)?;
        let transac_uid = transac.get::<&'static str>("uid")?;
        let query = match transac.optional::<JsoncObj>("query")? {
            Some(value) => value,
            None => JsoncObj::new(),
        };
        let expect = match transac.optional::<JsoncObj>("expect")? {
            Some(value) => {
                match context {
                    TransactionVerbCtx::Injector(injector) => {
                        if injector.get_minimal_mode() {
                            // in minimal mode only check response status
                            let status = value.default::<String>("rcode", "ok".to_string())?;
                            let jsonc = JsoncObj::new();
                            jsonc.add("rcode", &status)?;
                            jsonc
                        } else {
                            value
                        }
                    }
                    TransactionVerbCtx::Responder(_) => value,
                }
            }
            None => JsoncObj::new(),
        };

        // build verb from transaction uid
        let current_verb = transaction_get_verb(&transac)?;

        // ignore injector_only verbs as service discovery
        if transac.default("injector_only", false)? {
            afb_log_msg!(
                Notice,
                None,
                "uid:{} scenario:{} verb:{} ignored (injector_only==true)",
                uid_scenario,
                transac_uid,
                current_verb
            );
            continue;
        }

        if previous_verb != current_verb {
            // if exist create previous_label verb scenario
            if previous_verb.len() != 0 {
                create_transaction_verb(
                    scenario_group,
                    previous_verb,
                    infos,
                    queries,
                    expects,
                    callback,
                    context,
                    target,
                )?;
            }
            // prepare structure for new scenario verb
            infos = JsoncObj::array();
            queries = JsoncObj::array();
            expects = JsoncObj::array();
            previous_verb = current_verb;
        }

        infos.append(transac_uid)?;
        queries.append(query)?;
        expects.append(expect)?;
    }
    // add last verb
    create_transaction_verb(
        scenario_group,
        previous_verb,
        infos,
        queries,
        expects,
        callback,
        context,
        target,
    )?;

    Ok(scenario_group.finalize()?)
}

pub fn register_injector(
    api: &mut AfbApi,
    config: &BindingConfig,
) -> Result<Vec<&'static Injector>, AfbError> {
    scenario_actions::register()?;
    let mut injectors = Vec::new();

    match config.target {
        None => return afb_error!("register_injector", "target api SHOULD be defined"),
        Some(value) => {
            api.require_api(value);
        }
    }

    for idx in 0..config.scenarios.count()? {
        let jscenario = config.scenarios.index::<JsoncObj>(idx)?;

        let uid = match env::var("SCENARIO_UID") {
            Err(_) => format!("{}:{}", jscenario.get::<String>("uid")?, idx),
            Ok(value) => value,
        };
        let uid_scenario = to_static_str(uid);
        let name = jscenario.default("name", uid_scenario)?;
        let info = jscenario.default("info", "")?;
        let prefix = jscenario.default("prefix", uid_scenario)?;

        let transactions = jscenario.get::<JsoncObj>("transactions")?;
        if !transactions.is_type(Jtype::Array) {
            return afb_error!(
                "simu-injector-config",
                "transactions should be a valid array of (uid,request,expect)"
            );
        }
        let scenario_timeout = jscenario.default("timeout", transactions.count()? as u64)?;

        let scenario_event = AfbEvent::new(uid_scenario);
        let scenario_verb = AfbVerb::new(uid_scenario);
        let injector = Injector::new(
            uid_scenario,
            config.target,
            prefix,
            scenario_timeout,
            transactions.clone(),
            config.delay_conf,
            config.retry_conf,
            config.minimal_mode,
        )?;
        scenario_verb
            .set_name(name)
            .set_info(info)
            .set_actions("['start','stop','exec','result']")?
            .set_callback(scenario_action_cb)
            .set_context(ScenarioReqCtx {
                _uid: uid_scenario,
                job_id: 0,
                injector,
                evt: scenario_event,
            });
        api.add_verb(scenario_verb.finalize()?);
        api.add_event(scenario_event);

        // create a group by scenario with one verb per transaction
        if transactions.count()? > 0 {
            let transaction_group = create_transaction_group(
                transactions,
                uid_scenario,
                name,
                injector_req_cb,
                TransactionVerbCtx::Injector(injector),
                config.target,
            )?;
            api.add_group(transaction_group);
        }
        injectors.push(injector);
    }
    Ok(injectors)
}

fn responder_reset_cb(
    afb_rqt: &AfbRequest,
    _args: &AfbRqtData,
    ctx: &AfbCtxData,
) -> Result<(), AfbError> {
    let ctx = ctx.get_mut::<ResponderReset>()?;
    ctx.responder.reset();
    afb_rqt.reply(AFB_NO_DATA, 0);
    Ok(())
}

pub fn register_responder(api: &mut AfbApi, config: &BindingConfig) -> Result<(), AfbError> {
    let responder = Responder::new(config.loop_reset);
    let responder_verb = AfbVerb::new("reset")
        .set_info("scenario sequence counter")
        .set_callback(responder_reset_cb)
        .set_context(ResponderReset { responder })
        .finalize()?;
    api.add_verb(responder_verb);

    // create one group per scenario
    for idx in 0..config.scenarios.count()? {
        let jscenario = config.scenarios.index::<JsoncObj>(idx)?;
        let uid = match env::var("SCENARIO_UID") {
            Err(_) => format!("{}:{}", jscenario.get::<String>("uid")?, idx),
            Ok(value) => value,
        };
        let uid_scenario = to_static_str(uid);
        let name = jscenario.default::<&'static str>("name", uid_scenario)?;
        let transactions = jscenario.get::<JsoncObj>("transactions")?;
        if !transactions.is_type(Jtype::Array) {
            return afb_error!(
                "simu-injector-config",
                "transactions should be a valid array of (uid,request,expect)"
            );
        }

        // create a group by scenario with one verb per transaction
        if transactions.count()? > 0 {
            let transaction_group = create_transaction_group(
                transactions,
                uid_scenario,
                name,
                responder_req_cb,
                TransactionVerbCtx::Responder(responder),
                None,
            )?;
            api.add_group(transaction_group);
        }
    }
    Ok(())
}
