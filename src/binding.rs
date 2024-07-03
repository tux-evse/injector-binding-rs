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

pub enum SimulationMode {
    Responder,
    Injector,
}

pub struct BindingConfig {
    pub simulation: SimulationMode,
    pub scenarios: JsoncObj,
    pub target: Option<&'static str>,
}

// Binding init callback started at binding load time before any API exist
// -----------------------------------------
pub fn binding_init(_rootv4: AfbApiV4, jconf: JsoncObj) -> Result<&'static AfbApi, AfbError> {
    //afb_log_msg!(Info, rootv4, "config:{}", jconf);

    let uid = jconf.default("uid", "iso15118-simu")?;
    let api = jconf.default("api", uid)?;
    let info = jconf.default("info", "")?;

    let simulation = match jconf.default("simulation", "injector")? {
        "" | "injector" => SimulationMode::Injector,
        "responder" => SimulationMode::Responder,
        other => return afb_error! ("simu-binding-config", "expected mode:'injector'|'responder' got:{}", other)
    };

    let target=  jconf.optional::<&'static str>("target")?;

    let scenarios =    jconf.get::<JsoncObj>("scenarios")?;
    if ! scenarios.is_type(Jtype::Array) {
        return afb_error! ("simu-binding-config", "scenarios should be a valid array of simulator messages")
    }

    let config = BindingConfig {
        simulation,
        scenarios: scenarios.clone(),
        target,
    };
    // create an register frontend api and register init session callback
    let api = AfbApi::new(api)
        .set_info(info);


    // create verbs
    register_verbs(api, &config)?;

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
