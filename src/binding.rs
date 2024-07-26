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
use std::env;

pub enum SimulationMode {
    Responder,
    Injector,
}

pub struct BindingConfig {
    pub simulation: SimulationMode,
    pub scenarios: JsoncObj,
    pub target: Option<&'static str>,
    pub loop_reset: bool,
    pub delay_conf: InjectorDelayConf,
    pub retry_conf: InjectorRetryConf,
}

struct ApiInjectorCtx {
    injector: &'static Injector,
}

impl AfbApiControls for ApiInjectorCtx {
    // the API is created and ready. At this level user may subcall api(s) declare as dependencies
    fn start(&mut self, api: &AfbApi) -> Result<(), AfbError> {
        afb_log_msg!(
            Warning,
            api,
            "autorun started, scenario: {}",
            self.injector.get_uid()
        );
        let param = JobScenarioParam {
            injector: self.injector,
            event: None,
            api: api.get_apiv4(),
        };
        job_scenario_exec(&param)?;
        let result = self.injector.get_result()?;
        println!("{:#}", result);
        afb_log_msg!(Warning, api, "autorun exit");
        std::process::exit(0);
    }

    // mandatory unsed declaration
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

// Binding init callback started at binding load time before any API exist
// -----------------------------------------
pub fn binding_init(_rootv4: AfbApiV4, jconf: JsoncObj) -> Result<&'static AfbApi, AfbError> {
    //afb_log_msg!(Info, rootv4, "config:{}", jconf);

    let uid = jconf.default("uid", "iso15118-simu")?;
    let api = jconf.default("api", uid)?;
    let info = jconf.default("info", "")?;

    let smode = match env::var("SIMULATION_MODE") {
        Err(_) => jconf.default::<String>("simulation","injector".to_string())?,
        Ok(value) => value,
    };

    let simulation = match smode.to_lowercase().as_str() {
        "injector" => SimulationMode::Injector,
        "responder" => SimulationMode::Responder,
        other => {
            return afb_error!(
                "simu-binding-config",
                "expected mode:'injector'|'responder' got:{}",
                other
            )
        }
    };

    let loop_reset = jconf.default("loop", true)?;
    let target = jconf.optional::<&'static str>("target")?;

    let scenarios = jconf.get::<JsoncObj>("scenarios")?;
    if !scenarios.is_type(Jtype::Array) {
        return afb_error!(
            "simu-binding-config",
            "scenarios should be a valid array of simulator messages"
        );
    }

    let retry_conf = match jconf.optional::<JsoncObj>("retry")? {
        None => InjectorRetryConf::default(),
        Some(jretry) => InjectorRetryConf::from_jsonc(jretry)?,
    };

    let delay_conf = match jconf.optional::<JsoncObj>("delay")? {
        None => InjectorDelayConf::default(),
        Some(jretry) => InjectorDelayConf::from_jsonc(jretry)?,
    };

    let config = BindingConfig {
        simulation,
        scenarios: scenarios.clone(),
        target,
        loop_reset,
        delay_conf,
        retry_conf,
    };
    // create an register frontend api and register init session callback
    let api = AfbApi::new(api).set_info(info);

    match config.simulation {
        SimulationMode::Injector => {
            let injectors = register_injector(api, &config)?;
            let autostart = match env::var("AUTORUN") {
                Err(_) => jconf.default("autorun", 0)?,
                Ok(value) => match value.parse() {
                    Ok(number) => number,
                    Err(_) => 0,
                },
            };

            if autostart > 0 {
                if autostart >= injectors.len() as u32 + 1 {
                    return afb_error!(
                        "simu-binding-config",
                        "autostart invalid value:{} should be 1-{}",
                        autostart,
                        injectors.len()
                    );
                }

                let api_ctx = ApiInjectorCtx {
                    injector: injectors[(autostart - 1) as usize],
                };
                api.set_callback(Box::new(api_ctx));
            }
        }
        SimulationMode::Responder => {
            register_responder(api, &config)?;
        }
    }

    // if acls set apply them
    if let Ok(value) = jconf.get::<&'static str>("permission") {
        api.set_permission(AfbPermission::new(value));
    };

    if let Ok(value) = jconf.get::<i32>("verbosity") {
        api.set_verbosity(value);
    };

    Ok(api.finalize()?)
}

// register binding within libafb
AfbBindingRegister!(binding_init);
