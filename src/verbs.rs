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

const DEFAULT_ISO_TIMEOUT: i32 = 1000; // call_sync ms default timeout

AfbDataConverter!(scenario_actions, ScenarioAction);
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase", tag = "action")]
pub enum ScenarioAction {
    #[default]
    START,
    STOP,
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

fn cmp_entry<'a>(value: &'a Jentry, expect: &Jentry) -> Option<&'a Jentry> {
    if value.key == expect.key {
        Some(value)
    } else {
        None
    }
}

fn check_arguments(
    sequence: usize,
    received: &JsoncObj,
    expected: &JsoncObj,
) -> Result<SimulationStatus, AfbError> {
    // move from jsonc to a rust vector of json object
    let received = received.expand()?;
    let expected = expected.expand()?;

    if expected.len() == 0 {
        return Ok(SimulationStatus::Ignored);
    }

    for idx in 0..expected.len() {
        let expected_entry = &expected[idx];
        let received_entry = match received.iter().find_map(|s| cmp_entry(s, expected_entry)) {
            None => {
                return afb_error!(
                    "simu-check-response",
                    format!("seq:{} fail to find key:{}", sequence, expected_entry.key)
                )
            }
            Some(value) => value,
        };

        // if entry value embed a nested object let's recursively check content
        if received_entry.obj.is_type(Jtype::Object) {
            let response = check_arguments(sequence, &received_entry.obj, &expected_entry.obj);
            match check_arguments(sequence, &received_entry.obj, &expected_entry.obj)? {
                SimulationStatus::Check => {}
                SimulationStatus::Ignored => {}
                _ => return response,
            }
        }

        // check both received & expected value match
        if let Err(_error) = received_entry.obj.clone().equal(expected_entry.obj.clone()) {
            return afb_error!(
                "simu-check-response",
                "seq:{} fail key:'{}' value:{}!={}",
                sequence,
                expected_entry.key,
                expected_entry.obj,
                received_entry.obj
            );
        }
    }
    Ok(SimulationStatus::Check)
}

fn injector_req_cb(
    api: AfbApiV4,
    transac: &mut InjectorEntry,
    target: &'static str,
) -> Result<SimulationStatus, AfbError> {
    // injector_req_cb does not return AfbError
    let mut query = AfbParams::new();
    for idx in 0..transac.queries.count()? {
        let jsonc = transac.queries.index::<JsoncObj>(idx)?;
        query.push(jsonc.clone())?;
    }

    let response = AfbSubCall::call_sync(api, target, transac.verb, query)?;
    let status = match transac.expects.count()? {
        1 => {
            // injector only use 1st expect element
            let received = response.get::<JsoncObj>(0)?;
            check_arguments(transac.sequence, &received, &transac.expects.index(0)?)?
        }
        0 => SimulationStatus::Done,
        _ => {
            return afb_error!(
                "injector-req-cb",
                "(hoops) injection scenario with multiple expect return element"
            )
        }
    };

    Ok(status)
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
    let ctx = ctx.get_mut::<ScenarioReqCtx>()?;
    let action = args.get::<&ScenarioAction>(0)?;

    match action {
        ScenarioAction::START => {
            ctx.evt.subscribe(afb_rqt)?;
            ctx.job_id = ctx.injector.start(afb_rqt, ctx.evt)?;
            afb_rqt.reply(ctx.job_id, 0);
        }

        ScenarioAction::STOP => {
            ctx.evt.unsubscribe(afb_rqt)?;
            let result = ctx.injector.stop(ctx.job_id)?;
            afb_rqt.reply(result, 0);
            ctx.job_id = 0;
        }

        ScenarioAction::RESULT => {
            let result = ctx.injector.get_result()?;
            afb_rqt.reply(result, 0);
        }
    }

    Ok(())
}

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
        return afb_error!(
            "responder-req-cb",
            "invalid sequence number:{}",
            transac.sequence
        );
    };

    let received_query = args.get::<JsoncObj>(0)?;
    let expected_query = transac.queries.index(transac.sequence)?;

    let status = check_arguments(transac.sequence, &received_query, &expected_query)?;

    match status {
        SimulationStatus::Done | SimulationStatus::Check => {
            let expect = transac.expects.index::<JsoncObj>(transac.sequence)?;
            if expect.len()? == 0 {
                afb_rqt.reply(AFB_NO_DATA, 0);
            } else {
                afb_rqt.reply(expect, 0);
            }
        }

        error => {
            return afb_error!(
                "responder-req-fail",
                "argument check return invalid value:{:?}",
                error
            )
        }
    }
    // next run should match with next sequence transaction
    transac.sequence += 1;
    Ok(())
}

fn register_injector(api: &mut AfbApi, config: &BindingConfig) -> Result<(), AfbError> {
    scenario_actions::register()?;

    for idx in 0..config.scenarios.count()? {
        let jscenario = config.scenarios.index::<JsoncObj>(idx)?;
        let uid = jscenario.get::<&'static str>("uid")?;
        let name = jscenario.default::<&'static str>("name", uid)?;
        let info = jscenario.default::<&'static str>("info", "")?;
        let timeout = jscenario.default::<i32>("timeout", DEFAULT_ISO_TIMEOUT)?;
        let target = jscenario.get::<&'static str>("target")?;
        let transactions = jscenario.get::<JsoncObj>("transactions")?;
        if !transactions.is_type(Jtype::Array) {
            return afb_error!(
                "simu-injector-config",
                "transactions should be a valid array of (uid,request,expect)"
            );
        }

        let scenario_event = AfbEvent::new(uid);
        let scenario_verb = AfbVerb::new(uid);
        let injector = Injector::new(uid, target, transactions, timeout, injector_req_cb)?;
        scenario_verb
            .set_name(name)
            .set_info(info)
            .set_actions("['start','stop','result']")?
            .set_callback(scenario_action_cb)
            .set_context(ScenarioReqCtx {
                _uid: uid,
                job_id: 0,
                injector,
                evt: scenario_event,
            });
        api.add_verb(scenario_verb.finalize()?);
        api.add_event(scenario_event);
    }
    Ok(())
}

fn create_responder_verb(
    group: &mut AfbGroup,
    uid: &'static str,
    infos: JsoncObj,
    queries: JsoncObj,
    expects: JsoncObj,
    responder: &'static Responder,
) -> Result<(), AfbError> {
    let responder_verb = AfbVerb::new(uid)
        .set_info(to_static_str(infos.to_string()))
        .set_callback(responder_req_cb)
        .set_context(ResponderEntry {
            queries: queries.clone(),
            expects,
            sequence: 0,
            nonce:0,
            responder,
        });

    for idx in 0..queries.count()? {
        let info = infos.index::<&str>(idx)?;
        let jsonc = JsoncObj::new();
        jsonc.add("info", info)?;
        for entry in queries.index::<JsoncObj>(idx)?.expand()? {
            jsonc.add(&entry.key, entry.obj)?;
        }
        responder_verb.add_sample(jsonc)?;
    }
    group.add_verb(responder_verb.finalize()?);
    Ok(())
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

fn register_responder(api: &mut AfbApi, config: &BindingConfig) -> Result<(), AfbError> {
    let responder = Responder::new();
    let responder_verb = AfbVerb::new("reset")
        .set_info("scenario sequence counter")
        .set_callback(responder_reset_cb)
        .set_context(ResponderReset { responder })
        .finalize()?;
    api.add_verb(responder_verb);

    // create one group per scenario
    for idx in 0..config.scenarios.count()? {
        let jscenario = config.scenarios.index::<JsoncObj>(idx)?;
        let uid_scenario = jscenario.get::<&'static str>("uid")?;
        let name = jscenario.default::<&'static str>("name", uid_scenario)?;
        let transactions = jscenario.get::<JsoncObj>("transactions")?;
        if !transactions.is_type(Jtype::Array) {
            return afb_error!(
                "simu-injector-config",
                "transactions should be a valid array of (uid,request,expect)"
            );
        }

        // ignore empty scenario
        if transactions.count()? > 0 {
            let scenario_group = AfbGroup::new(uid_scenario).set_prefix(name);

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
                    Some(value) => value,
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
                        create_responder_verb(
                            scenario_group,
                            previous_verb,
                            infos,
                            queries,
                            expects,
                            responder,
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
            create_responder_verb(
                scenario_group,
                previous_verb,
                infos,
                queries,
                expects,
                responder,
            )?;
            api.add_group(scenario_group.finalize()?);
        }
    }
    Ok(())
}

pub fn register_verbs(api: &mut AfbApi, config: &BindingConfig) -> Result<(), AfbError> {
    match config.simulation {
        SimulationMode::Injector => register_injector(api, config),
        SimulationMode::Responder => register_responder(api, config),
    }
}
