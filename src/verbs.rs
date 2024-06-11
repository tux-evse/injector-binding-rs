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

fn cmp_entry<'a>(value: &Jentry, expect: &'a Jentry) -> Option<&'a Jentry> {
    if value.key == expect.key {
        Some(expect)
    } else {
        None
    }
}

fn check_arguments(received: &JsoncObj, expected: &JsoncObj) -> Result<SimulationStatus, AfbError> {
    // move from jsonc to a rust vector of json object
    let expected = expected.expand()?;
    let received = received.expand()?;

    for idx in 0..expected.len() {
        let expect_entry = &expected[idx];
        let reply_entry = match received.iter().find_map(|s| cmp_entry(s, expect_entry)) {
            None => {
                return afb_error!(
                    "simu-check-response",
                    format!("fail to find key:{}", expect_entry.key)
                )
            }
            Some(value) => value,
        };

        // if entry value embed a nested object let's recursively check content
        if reply_entry.obj.is_type(Jtype::Object) {
            let response = check_arguments(&reply_entry.obj, &expect_entry.obj);
            match check_arguments(&reply_entry.obj, &expect_entry.obj)? {
                SimulationStatus::Check => {}
                SimulationStatus::Done => {}
                _ => return response,
            }
        }

        // check both received & expected value match
        if let Err(error) = reply_entry.obj.clone().equal(expect_entry.obj.clone()) {
            return afb_error!(
                "simu-check-response",
                format!(
                    "fail key:{} value:{}!={} {}",
                    expect_entry.key, expect_entry.obj, reply_entry.obj, error
                )
            );
        }
    }
    Ok(SimulationStatus::Check)
}

fn injector_req_cb(
    api: AfbApiV4,
    transac: &mut TransacEntry,
    target: &'static str,
) -> Result<SimulationStatus, AfbError> {
    // injector_req_cb does not return AfbError
    let mut query = AfbParams::new();
    if let Some(jsonc) = &transac.query {
        query.push(jsonc.clone())?;
    }

    let response= AfbSubCall::call_sync(api, target, transac.verb, query)?;
    let status = match transac.expect.clone() {
        None => SimulationStatus::Done,
        Some(expect) => {
            let received = response.get::<JsoncObj>(0)?;
            check_arguments(&received, &expect)?
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
    let transac = ctx.get_mut::<TransacEntry>()?;

    let status = match &transac.query {
        None => SimulationStatus::Done,
        Some(expect) => {
            let jsonc = args.get::<JsoncObj>(0)?;
            check_arguments(&jsonc, &expect)?
        }
    };

    match status {
        SimulationStatus::Done | SimulationStatus::Check => match &transac.expect {
            None => afb_rqt.reply(AFB_NO_DATA, 0),
            Some(value) => afb_rqt.reply(value.clone(), 0),
        },
        error => {
            return afb_error!(
                "responder-req-fail",
                "argument check return invalid value:{:?}",
                error
            )
        }
    }
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

fn register_responder(api: &mut AfbApi, config: &BindingConfig) -> Result<(), AfbError> {
    // create one group per scenario
    for idx in 0..config.scenarios.count()? {
        let jscenario = config.scenarios.index::<JsoncObj>(idx)?;
        let uid_scenario = jscenario.get::<&'static str>("uid")?;
        let name = jscenario.default::<&'static str>("name", uid_scenario)?;
        let info = jscenario.default::<&'static str>("info", "")?;
        //let target = jscenario.get::<&'static str>("target")?;
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

            for idx in 0..transactions.count()? {
                let transac = transactions.index::<JsoncObj>(idx)?;

                let uid_transac = transac.get::<&'static str>("uid")?;
                let query = transac.optional::<JsoncObj>("query")?;
                let expect = transac.optional::<JsoncObj>("expect")?;
                let verb = match transac.optional::<&'static str>("verb")? {
                    Some(value) => value,
                    None => {
                        let name = format!("{}_req", uid_transac.replace("-", "_"));
                        to_static_str(name)
                    }
                };

                // if query/action define let's ignore verb
                if let Some(jquery) = &query {
                    if let Some(_jaction) = jquery.optional::<JsoncObj>("action")? {
                        afb_log_msg!(
                            Notice,
                            None,
                            "uid:{} scenario:{} verb:{} ignored (action defined)",
                            uid_scenario,
                            uid_transac,
                            verb
                        );
                        continue;
                    }
                }

                let responder_verb = AfbVerb::new(uid_transac)
                    .set_name(verb)
                    .set_info(info)
                    .set_callback(responder_req_cb)
                    .set_context(TransacEntry {
                        uid: uid_transac,
                        verb,
                        query: query.clone(),
                        expect,
                        status: SimulationStatus::Idle,
                    });

                if let Some(sample) = query {
                    responder_verb.set_sample(sample)?;
                }
                scenario_group.add_verb(responder_verb.finalize()?);
            }
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
