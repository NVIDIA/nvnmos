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

#include <cstdint>
#include <functional>
#include <vector>

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
    typedef utility::string_t channelmapping_id;

    struct node_model;

    namespace experimental
    {
        struct node_implementation;
    }
}

namespace nvnmos
{
    // caller-chosen resource identity (x-nvnmos-name / urn:x-nvnmos:tag:name)
    typedef utility::string_t name;

    namespace impl
    {
        // generate repeated ids for the node's resources
        nmos::id make_id(const nmos::id& seed_id, const nmos::type& type, const nvnmos::name& name = {});

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
        const web::json::field_as_value channelmappings{ U("channelmappings") }; // keyed by caller-chosen channel mapping name
        const web::json::field_as_value channelmapping_inputs{ U("inputs") }; // array of IS-08 input ids
        const web::json::field_as_value channelmapping_outputs{ U("outputs") }; // array of IS-08 output ids
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

    // exception type indicating the node model is in an inconsistent state
    struct node_implementation_exception {};

    // Dense IS-08 active map for one output; index i is output channel i (same type as nmos::make_channelmapping_active_map).
    typedef std::vector<std::pair<nmos::channelmapping_id, uint32_t>> channelmapping_active_map;

    // IS-08 input geometry for node_implementation_add_channelmapping.
    struct channelmapping_input
    {
        nmos::channelmapping_id id;
        utility::string_t name;
        utility::string_t description;
        std::vector<utility::string_t> channel_labels;
        nvnmos::name parent_name;
        // nmos::types::receiver or nmos::types::source when parent_name is set; ignored otherwise.
        nmos::type parent_type;
        // Input /caps reordering; libnvnmos default true (most flexible for software routing).
        bool reordering = true;
        // Input /caps block_size; libnvnmos default 1 (most flexible for software routing).
        unsigned int block_size = 1;
    };

    // IS-08 output geometry for node_implementation_add_channelmapping.
    struct channelmapping_output
    {
        nmos::channelmapping_id id;
        utility::string_t name;
        utility::string_t description;
        std::vector<utility::string_t> channel_labels;
        nvnmos::name sender_name;
        std::vector<nmos::channelmapping_id> routable_inputs;
    };

    // Input/output bundle for node_implementation_add_channelmapping.
    struct channelmapping_config
    {
        std::vector<channelmapping_input> inputs;
        std::vector<channelmapping_output> outputs;
    };

    // This constructs and inserts a node resource and a device resource into the model, based on the model settings.
    void node_implementation_init(nmos::node_model& model, slog::base_gate& gate);

    // This constructs and inserts sources/flows/senders into the model, based on the specified transport file.
    void node_implementation_add_sender(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate);

    // This removes sources/flows/senders from the model corresponding to the specified name.
    void node_implementation_remove_sender(nmos::node_model& model, const nvnmos::name& sender_name, slog::base_gate& gate);

    // This constructs and inserts a receiver into the model, based on the specified transport file.
    void node_implementation_add_receiver(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate);

    // This removes the receiver from the model corresponding to the specified name.
    void node_implementation_remove_receiver(nmos::node_model& model, const nvnmos::name& receiver_name, slog::base_gate& gate);

    // This is an application callback to update the specified sender or receiver, as a result of an IS-05 Connection API activation.
    // `type` is `nmos::types::sender` or `nmos::types::receiver` and disambiguates `name`, which is unique within the given side on the Node.
    // The transport file is the updated SDP (for nmos::transports::rtp) or the updated MXL flow definition JSON (for nmos::transports::mxl).
    // If the transport file is empty, the sender or receiver has been deactivated.
    typedef std::function<void(const nmos::type& type, const nvnmos::name& name, const std::string& transport_file)> connection_activation_handler;

    // This constructs and inserts IS-08 input/output resources into the model, based on the specified channel mapping configuration.
    void node_implementation_add_channelmapping(nmos::node_model& model, const nvnmos::name& name, const channelmapping_config& mapping, slog::base_gate& gate);

    // This removes IS-08 input/output resources from the model corresponding to the specified name.
    void node_implementation_remove_channelmapping(nmos::node_model& model, const nvnmos::name& name, slog::base_gate& gate);

    // This is an application callback to apply the active map for the specified output, as a result of an IS-08 Channel Mapping API activation.
    // `name` is the caller-chosen channel mapping name (unique per Node). `output_id` is the IS-08 output id just activated.
    // `active_map` is dense per output channel index; unrouted channels have an empty input id (empty pair.first).
    typedef std::function<bool(const nvnmos::name& name, const nmos::channelmapping_id& output_id, const channelmapping_active_map& active_map)> channelmapping_activation_handler;

    // This constructs all the callbacks used to integrate the application into the server instance for the NMOS Node.
    nmos::experimental::node_implementation make_node_implementation(nmos::node_model& model, connection_activation_handler connection_activated, channelmapping_activation_handler channelmapping_activated, slog::base_gate& gate);

    // This updates the transport parameters and transport file for the specified sender or receiver based on the specified transport file.
    // `type` selects between a sender and a receiver with the same `name` on the Node.
    // For now, the transport file is not validated against the existing sender or receiver capabilities and constraints.
    // Does not invoke the application's connection activation callback.
    void node_implementation_activate_connection(nmos::node_model& model, const nmos::type& type, const nvnmos::name& name, const std::string& transport_file, slog::base_gate& gate);

    // This updates the published active map for the specified output in the channel mapping, based on the specified active map.
    // `output_id` is the IS-08 output id. `active_map` length must equal that output's channel count; index i is output channel i.
    // Does not invoke the application's channel mapping activation callback.
    void node_implementation_activate_channelmapping(nmos::node_model& model, const nvnmos::name& name, const nmos::channelmapping_id& output_id, const channelmapping_active_map& active_map, slog::base_gate& gate);
}

#endif
