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
#include "nmos/id.h"
#include "nmos/settings.h"
#include "nmos/transport.h"
#include "nmos/type.h"

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
    namespace impl
    {
        // generate repeated ids for the node's resources
        nmos::id make_id(const nmos::id& seed_id, const nmos::type& type, const utility::string_t& name = {});

        // generate URLs for the Node API and Connection API
        std::pair<utility::string_t, utility::string_t> make_api_base_urls(const nmos::settings& settings);
    }

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
        const web::json::field_as_string transport{ U("transport") };
        const web::json::field_as_string transport_file{ U("transport_file") };

        const web::json::field_as_value clocks{ U("clocks") }; // object with clock names as keys

        const web::json::field_as_value interface_bindings{ U("interface_bindings") }; // object with interface names as keys, reference counts as values
    }

    // custom SDP attributes
    namespace attributes
    {
        // for senders and receivers
        const utility::string_t name{ U("x-nvnmos-name") };
        const utility::string_t group_hint{ U("x-nvnmos-group-hint") };
        const utility::string_t interface_ip{ U("x-nvnmos-iface-ip") };
        const utility::string_t interface{ U("x-nvnmos-iface") };
        // for receivers
        const utility::string_t caps{ U("x-nvnmos-caps") };
        // for senders
        const utility::string_t source_port{ U("x-nvnmos-src-port") };
    }

    // extra MXL flow definition properties
    namespace fields
    {
        const web::json::field_as_integer channel_count{ U("channel_count") };
    }

    // custom NvNmos tag fields
    //
    // These name entries in the `tags` property -- both within an MXL flow
    // definition JSON document and on an NMOS resource.
    // They follow the same tag URN convention as the standard
    // `urn:x-nmos:tag:grouphint/v1.0` group hint.
    //
    // `name` and `mxl_domain_id` values are single-element arrays
    // holding a non-empty string; readers return the first entry.
    //
    // `caps` carries a single-element array whose first entry describes
    // the receiver's capability advertisement: an empty string means
    // "fully flexible" (format-derived capabilities omitted). Non-empty
    // strings are reserved for future use (e.g. constraint expressions);
    // today any non-empty array is treated as fully flexible.
    namespace fields
    {
        const web::json::field_as_value_or name{ U("urn:x-nvnmos:tag:name"), web::json::value::array() };
        const web::json::field_as_value_or caps{ U("urn:x-nvnmos:tag:caps"), web::json::value::array() };
        const web::json::field_as_value_or mxl_domain_id{ U("urn:x-nvnmos:tag:mxl-domain-id"), web::json::value::array() };
    }

    // custom SDP format-specific parameters
    namespace fields
    {
        const web::json::field<uint64_t> transport_bit_rate{ U("x-nvnmos-transport-bit-rate") }; // transport bit rate including IP/UDP/RTP overhead
        const web::json::field<uint64_t> format_bit_rate{ U("x-nvnmos-format-bit-rate") }; // format bit rate excluding IP/UDP/RTP overhead
    }

    struct node_implementation_exception {};

    // This constructs and inserts a node resource and a device resource into the model, based on the model settings.
    void node_implementation_init(nmos::node_model& model, slog::base_gate& gate);

    // This constructs and inserts sources/flows/senders into the model, based on the specified transport file.
    void node_implementation_add_sender(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate);

    // This removes sources/flows/senders from the model corresponding to the specified name.
    void node_implementation_remove_sender(nmos::node_model& model, const utility::string_t& sender_name, slog::base_gate& gate);

    // This constructs and inserts a receiver into the model, based on the specified transport file.
    void node_implementation_add_receiver(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate);

    // This removes the receiver from the model corresponding to the specified name.
    void node_implementation_remove_receiver(nmos::node_model& model, const utility::string_t& receiver_name, slog::base_gate& gate);

    // This is an application callback to update the specified sender or receiver, as a result of an IS-05 Connection API activation.
    // `type` is `nmos::types::sender` or `nmos::types::receiver` and disambiguates `name`, which is unique within the given side on the Node.
    // The transport file is the updated SDP (for nmos::transports::rtp) or the updated MXL flow definition JSON (for nmos::transports::mxl).
    // If the transport file is empty, the sender or receiver has been deactivated.
    typedef std::function<void(const nmos::type& type, const std::string& name, const std::string& transport_file)> connection_activation_handler;

    // This constructs all the callbacks used to integrate the application into the server instance for the NMOS Node.
    nmos::experimental::node_implementation make_node_implementation(nmos::node_model& model, connection_activation_handler connection_activated, slog::base_gate& gate);

    // This updates the transport parameters and transport file for the specified sender or receiver based on the specified transport file.
    // `type` selects between a sender and a receiver with the same `name` on the Node.
    // For now, the transport file is not validated against the existing sender or receiver capabilities and constraints.
    void node_implementation_activate_connection(nmos::node_model& model, const nmos::type& type, const utility::string_t& name, const std::string& transport_file, slog::base_gate& gate);
}

#endif
