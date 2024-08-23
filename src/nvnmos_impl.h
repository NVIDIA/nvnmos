/*
 * SPDX-FileCopyrightText: Copyright (c) 2022-2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

/*
 Portions of this software are derived from the following third party software.

 nmos-cpp: An NMOS C++ Implementation
 Copyright (c) 2017 Sony Corporation. All Rights Reserved.
 Licensed under the Apache License, Version 2.0 (the "License").
 */

#ifndef NVNMOS_IMPL_H
#define NVNMOS_IMPL_H

#include "cpprest/json_utils.h"

namespace slog
{
    class base_gate;
}

namespace nmos
{
    struct node_model;

    namespace experimental
    {
        struct node_implementation;
    }
}

namespace nvnmos
{
    // custom settings fields
    namespace fields
    {
        const web::json::field_as_string_or node_label{ U("node_label"), U("") };
        const web::json::field_as_string_or node_description{ U("node_description"), U("") };
        const web::json::field_as_value_or node_tags{ U("node_tags"), web::json::value::object() };

        const web::json::field_as_string_or device_label{ U("device_label"), U("") };
        const web::json::field_as_string_or device_description{ U("device_description"), U("") };
        const web::json::field_as_value_or device_tags{ U("device_tags"), web::json::value::object() };

        const web::json::field_as_value senders{ U("senders") }; // object with ids as keys
        const web::json::field_as_value receivers{ U("receivers") }; // object with ids as keys
        const web::json::field_as_string sdp{ U("sdp") };

        const web::json::field_as_value clocks{ U("clocks") }; // object with clock names as keys
    }

    // custom SDP attributes
    namespace attributes
    {
        // for senders and receivers
        const utility::string_t internal_id{ U("x-nvnmos-id") };
        const utility::string_t group_hint{ U("x-nvnmos-group-hint") };
        // for receivers
        const utility::string_t interface_ip{ U("x-nvnmos-iface-ip") };
        // for senders
        const utility::string_t source_port{ U("x-nvnmos-src-port") };
    }

    struct node_implementation_exception {};

    // This constructs and inserts a node resource and a device resource into the model, based on the model settings.
    void node_implementation_init(nmos::node_model& model, slog::base_gate& gate);

    // This constructs and inserts sources/flows/senders into the model, based on the specified SDP file.
    void node_implementation_add_sender(nmos::node_model& model, const std::string& sdp, slog::base_gate& gate);

    // This removes sources/flows/senders from the model corresponding to the specified id.
    void node_implementation_remove_sender(nmos::node_model& model, const utility::string_t& id, slog::base_gate& gate);

    // This constructs and inserts a receiver into the model, based on the specified SDP file.
    void node_implementation_add_receiver(nmos::node_model& model, const std::string& sdp, slog::base_gate& gate);

    // This removes the receiver from the model corresponding to the specified id.
    void node_implementation_remove_receiver(nmos::node_model& model, const utility::string_t& id, slog::base_gate& gate);

    // This is an application callback to update the specified sender or receiver, as a result of an IS-05 Connection API activation.
    // If the SDP file is empty, the sender or receiver has been deactivated.
    typedef std::function<void(const std::string& id, const std::string& sdp)> rtp_connection_activation_handler;

    // This constructs all the callbacks used to integrate the application into the server instance for the NMOS Node.
    nmos::experimental::node_implementation make_node_implementation(nmos::node_model& model, rtp_connection_activation_handler rtp_connection_activated, slog::base_gate& gate);

    // This updates the transport parameters and transport file for the specified sender or receiver based on the specified SDP file.
    // For now, the SDP file is not validated against the existing sender or receiver capabilities and constraints.
    void node_implementation_activate_rtp_connection(nmos::node_model& model, const utility::string_t& id, const std::string& sdp, slog::base_gate& gate);
}

#endif
