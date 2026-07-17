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

#include "nvnmos_impl.h"

#include <boost/algorithm/string/case_conv.hpp>
#include <boost/algorithm/string/classification.hpp>
#include <boost/algorithm/string/predicate.hpp>
#include <boost/algorithm/string/replace.hpp>
#include <boost/algorithm/string/split.hpp>
#include <boost/asio/ip/address_v4.hpp>
#include <boost/range/adaptor/filtered.hpp>
#include <boost/range/adaptor/map.hpp>
#include <boost/range/adaptor/transformed.hpp>
#include <boost/range/algorithm/find.hpp>
#include <boost/range/algorithm/find_if.hpp>
#include <boost/range/irange.hpp>
#include "cpprest/host_utils.h"
#include "cpprest/regex_utils.h"
#include "cpprest/uri_builder.h"
#include "nmos/activation_mode.h"
#include "nmos/activation_utils.h"
#include "nmos/api_utils.h"
#include "nmos/capabilities.h"
#include "nmos/channels.h"
#include "nmos/channelmapping_resources.h"
#include "nmos/clock_name.h"
#include "nmos/clock_ref_type.h"
#include "nmos/colorspace.h"
#include "nmos/connection_resources.h"
#include "nmos/format.h"
#include "nmos/group_hint.h"
#include "nmos/interlace_mode.h"
#include "nmos/is04_versions.h"
#include "nmos/is05_versions.h"
#include "nmos/is08_versions.h"
#include "nmos/media_type.h"
#include "nmos/model.h"
#include "nmos/node_interfaces.h"
#include "nmos/node_resource.h"
#include "nmos/node_resources.h"
#include "nmos/node_server.h"
#include "nmos/sdp_utils.h"
#include "nmos/slog.h"
#include "nmos/st2110_21_sender_type.h"
#include "nmos/system_resources.h"
#include "nmos/transfer_characteristic.h"
#include "nmos/video_jxsv.h"
#include "sdp/sdp.h"

namespace nvnmos
{
    // node implementation details
    namespace impl
    {
        // supported formats
        enum class format
        {
            // video/raw or video/jxsv
            video,
            // audio/L24 or audio/L16
            audio,
            // video/smpte291
            data,
            // video/SMPTE2022-6
            mux
        };

        // identify supported format from media type
        format get_format(const nmos::media_type& media_type);

        // like nmos::make_session_description for 'internal' use
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value make_session_description(const nmos::type& type, const nvnmos::name& name, const utility::string_t& group_hint, const utility::string_t& session_info, const nmos::sdp_parameters& sdp_params, const web::json::value& transport_params, bool caps);

        // like nmos::get_session_description_sdp_parameters
        // with support for multiple ts-refclk attributes in each media description
        std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>> get_session_description_ts_refclks(const web::json::value& session_description);

        // like nmos::get_session_description_transport_params
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value get_session_description_transport_params(const nmos::type& type, const web::json::value& session_description);

        // get the (required) NvNmos resource name from the `x-nvnmos-name` custom attribute (not the SDP `s=` session-name line); throws std::invalid_argument if absent or empty
        nvnmos::name get_session_description_resource_name(const web::json::value& session_description);

        // get the optional group hint from the custom attribute
        utility::string_t get_session_description_group_hint(const web::json::value& session_description);

        // get the optional session information
        utility::string_t get_session_description_session_info(const web::json::value& session_description);

        // get the optional capabilities from the custom attribute
        bool has_session_description_caps(const web::json::value& session_description);

        // whether the IS-04 receiver has no BCP-004-01 constraint_sets (unconstrained)
        bool has_no_receiver_caps(const web::json::value& receiver);

        // get the format bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_format_bit_rate(const nmos::sdp_parameters& sdp_params);
        // get the transport bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_transport_bit_rate(const nmos::sdp_parameters& sdp_params);

        // find interface with the specified address
        std::vector<web::hosts::experimental::host_interface>::const_iterator find_interface(const std::vector<web::hosts::experimental::host_interface>& interfaces, const utility::string_t& address);

        // parse an x-nvnmos-iface attribute value into node interface details
        nmos::node_interface parse_iface(const utility::string_t& iface);
        // format node interface details as an x-nvnmos-iface attribute value
        utility::string_t make_iface(const nmos::node_interface& interface);
        // parse x-nvnmos-iface from media legs that carry the attribute; legs is IS-05 transport leg count (0 to skip leg-count check)
        std::vector<nmos::node_interface> get_session_description_interfaces(const web::json::value& session_description, size_t legs = 0);
        // get node interfaces from host_interfaces for interface_bindings
        std::map<utility::string_t, nmos::node_interface> get_interfaces_for_bindings(const std::vector<utility::string_t>& interface_names, const std::vector<web::hosts::experimental::host_interface>& host_interfaces);
        // look up the interface name from a transport param address via host_interfaces
        utility::string_t get_interface_name(const nmos::type& type, const web::json::value& transport_param, const std::vector<web::hosts::experimental::host_interface>& host_interfaces);

        // make a transport settings entry for a sender/receiver
        web::json::value make_transport_settings(const nmos::transport& transport, const utility::string_t& transport_file);

        // generate a repeatable source-specific multicast address for each leg of a sender
        utility::string_t make_source_specific_multicast_address_v4(const nmos::id& id, int leg);

        // generate URLs for a sender or receiver in the Node API and Connection API
        std::pair<utility::string_t, utility::string_t> make_resource_api_urls(const nmos::settings& settings, const nmos::id& id, const nmos::type& type);

        // insert a resource into the model, logging the outcome (cf. nmos-cpp-node's node_implementation)
        // throws node_implementation_exception if the resource could not be inserted, e.g. because of an unexpected duplicate id
        void insert_resource(nmos::resources& resources, nmos::resource&& resource, slog::base_gate& gate);

        // set the name for the sender or receiver as a resource tag
        void set_name(nmos::resource& resource, const nvnmos::name& name);
        // get the name for the sender or receiver from a resource tag
        nvnmos::name get_name(const nmos::resource& resource);

        // set the group hint for the sender or receiver as a resource tag
        void set_group_hint(nmos::resource& resource, const utility::string_t& group_hint);
        // get the group hint for the sender or receiver from a resource tag
        utility::string_t get_group_hint(const nmos::resource& resource);

        // find the source for the flow referenced by the source
        nmos::resources::const_iterator find_source_for_sender(const nmos::resources& resources, const nmos::resource& sender);

        // make a node clock based on specified SDP ts-refclk attributes and get the PTP domain if present
        web::json::value make_node_clock(const nmos::clock_name& clock_name, const std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>>& ts_refclks, int& ptp_domain);

        // modify node resource if necessary to update specified clock, which must already exist
        void update_node_clock(nmos::resources& node_resources, const nmos::id& node_id, const web::json::value& clock);

        struct interface_bindings_update
        {
            std::vector<utility::string_t> removed;
            std::vector<utility::string_t> added;
        };

        // modify node resource interfaces incrementally, maintaining interface_bindings reference counts in settings
        void update_node_interfaces(nmos::resources& node_resources, const nmos::id& node_id, const interface_bindings_update& bindings_update, const std::map<utility::string_t, nmos::node_interface>& interfaces, nmos::settings& settings);

        // resolve "auto" in connection transport params
        void resolve_auto(const nmos::resource& resource, const nmos::resource& connection_resource, web::json::value& transport_params, const utility::string_t& transport_file = {});
    }

    namespace impl
    {
        // parse an MXL flow definition (JSON) including nvnmos extensions
        // (urn:x-nvnmos:tag:* entries inside the `tags` property);
        // propagates web::json::json_exception on parse error
        web::json::value parse_mxl_flow_def(const std::string& flow_def);
        // extract the first string from the named tag in the flow definition's
        // `tags` property (or empty if absent)
        utility::string_t get_mxl_flow_def_tag(const web::json::value& flow_def, const web::json::field_as_value_or& tag_field);
        // extract the (required) name from the urn:x-nvnmos:tag:name tag;
        // throws std::invalid_argument if absent or empty
        nvnmos::name get_mxl_flow_def_name(const web::json::value& flow_def);
        // extract an optional group hint from the urn:x-nmos:tag:grouphint/v1.0 tag (or empty)
        utility::string_t get_mxl_flow_def_group_hint(const web::json::value& flow_def);
        // returns true if the urn:x-nvnmos:tag:caps tag asks for an unconstrained
        // receiver (format-derived capabilities omitted)
        bool has_mxl_flow_def_caps(const web::json::value& flow_def);
        // extract the MXL domain id from the urn:x-nvnmos:tag:mxl-domain-id tag;
        // returns empty when application-resolved (tag absent, empty array, or empty string)
        utility::string_t get_mxl_flow_def_domain_id(const web::json::value& flow_def);
        // resolve IS-05 mxl_domain_id "auto" from constraint: enum front or null
        web::json::value resolve_mxl_domain_id(const web::json::value& constraint);
        // extract the top-level 'id' property (or empty)
        utility::string_t get_mxl_flow_def_id(const web::json::value& flow_def);
        // produce a flow definition JSON string with the active MXL transport parameters spliced in
        std::string make_mxl_flow_def(web::json::value flow_def, const utility::string_t& mxl_domain_id, const utility::string_t& mxl_flow_id);
    }

    void node_implementation_init_(nmos::resources& node_resources, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);

        // for now, only manage a single clock
        const auto clock = nmos::clock_names::clk0;

        // node
        {
            const auto clocks = value_of({ nmos::make_internal_clock(clock) });
            auto node = nmos::make_node(node_id, clocks, {}, settings);
            node.data[nmos::fields::label] = value::string(nvnmos::fields::node_label(settings));
            node.data[nmos::fields::description] = value::string(nvnmos::fields::node_description(settings));
            node.data[nmos::fields::tags] = nvnmos::fields::node_tags(settings);
            impl::insert_resource(node_resources, std::move(node), gate);
        }

        // device
        {
            // omit IS-08 controls until the first channel mapping
            nmos::settings device_settings = settings;
            if (0 <= nmos::fields::channelmapping_port(settings))
            {
                device_settings[nmos::fields::channelmapping_port] = -1;
            }
            auto device = nmos::make_device(device_id, node_id, {}, {}, device_settings);
            device.data[nmos::fields::label] = value::string(nvnmos::fields::device_label(settings));
            device.data[nmos::fields::description] = value::string(nvnmos::fields::device_description(settings));
            device.data[nmos::fields::tags] = nvnmos::fields::device_tags(settings);
            impl::insert_resource(node_resources, std::move(device), gate);
        }

        // insert empty clock, sender, receiver, channelmapping and interface_binding configs
        settings[nvnmos::fields::clocks] = value::object();
        settings[nvnmos::fields::senders] = value::object();
        settings[nvnmos::fields::receivers] = value::object();
        settings[nvnmos::fields::channelmappings] = value::object();
        settings[nvnmos::fields::interface_bindings] = value::object();
    }

    void node_implementation_add_rtp_sender_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& sdp_, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto sdp = sdp::parse_session_description(sdp_);
        const auto sdp_params = nmos::get_session_description_sdp_parameters(sdp);
        const auto ts_refclks = impl::get_session_description_ts_refclks(sdp);
        const auto transport_params = impl::get_session_description_transport_params(nmos::types::sender, sdp);
        const auto name = impl::get_session_description_resource_name(sdp);
        // hm, could check the name is unique across all senders
        const auto group_hint = impl::get_session_description_group_hint(sdp);
        const auto session_info = impl::get_session_description_session_info(sdp);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto source_id = impl::make_id(seed_id, nmos::types::source, name);
        const auto flow_id = impl::make_id(seed_id, nmos::types::flow, name);
        const auto sender_id = impl::make_id(seed_id, nmos::types::sender, name);

        // for now, only manage a single clock
        const auto clock = nmos::clock_names::clk0;

        const auto media_type = nmos::get_media_type(sdp_params);
        const auto format = impl::get_format(media_type);

        const auto interfaces = impl::get_session_description_interfaces(sdp, transport_params.size());
        const auto interface_names = boost::copy_range<std::vector<utility::string_t>>(
            boost::irange(0, (int)transport_params.size()) | boost::adaptors::transformed([&](const int& leg)
            {
                if (!interfaces.empty()) return interfaces.at(leg).name;
                return impl::get_interface_name(nmos::types::sender, transport_params.at(leg), host_interfaces);
            }));

        nmos::resource source;
        nmos::resource flow;

        if (impl::format::video == format)
        {
            if (nmos::media_types::video_raw == media_type)
            {
                const auto video = nmos::get_video_raw_parameters(sdp_params);

                source = nmos::make_video_source(source_id, device_id, clock, video.exactframerate, settings);
                flow = nmos::make_raw_video_flow(
                    flow_id, source_id, device_id,
                    video.exactframerate,
                    video.width, video.height, video.interlace ? nmos::interlace_modes::interlaced_tff : nmos::interlace_modes::progressive,
                    nmos::colorspace{ video.colorimetry.name }, nmos::transfer_characteristic{ video.tcs.name }, video.sampling, video.depth,
                    settings
                );
            }
            else if (nmos::media_types::video_jxsv == media_type)
            {
                const auto video = nmos::get_video_jxsv_parameters(sdp_params);
                const auto format_bit_rate = impl::get_format_bit_rate(sdp_params);

                source = nmos::make_video_source(source_id, device_id, clock, video.exactframerate, settings);
                // nmos::make_video_jxsv_flow currently takes bits_per_pixel not bit_rate
                flow = nmos::make_video_jxsv_flow(
                    flow_id, source_id, device_id,
                    video.exactframerate,
                    video.width, video.height, video.interlace ? nmos::interlace_modes::interlaced_tff : nmos::interlace_modes::progressive,
                    nmos::colorspace{ video.colorimetry.name }, nmos::transfer_characteristic{ video.tcs.name }, video.sampling, video.depth,
                    nmos::profile{ video.profile.name }, nmos::level{ video.level.name }, nmos::sublevel{ video.sublevel.name }, 0,
                    settings
                );
                flow.data[nmos::fields::bit_rate] = value(format_bit_rate);
            }
        }
        else if (impl::format::audio == format)
        {
            const auto audio = nmos::get_audio_L_parameters(sdp_params);

            // hm, if present, should parse audio.channel_order into the equivalent vector of nmos::channel
            // but currently no nmos::parse_fmtp_channel_order
            const auto channels = boost::copy_range<std::vector<nmos::channel>>(
                boost::irange(0, (int)audio.channel_count) | boost::adaptors::transformed([&](const int& index)
            {
                return nmos::channel{ U(""), nmos::channel_symbols::Undefined(1 + index) };
            }));

            // hmm, should this take account of audio.packet_time?
            const nmos::rational grain_rate = audio.sample_rate;

            source = nmos::make_audio_source(source_id, device_id, clock, grain_rate, channels, settings);
            flow = nmos::make_raw_audio_flow(flow_id, source_id, device_id, audio.sample_rate, audio.bit_depth, settings);
            flow.data[nmos::fields::grain_rate] = nmos::make_rational(grain_rate);
        }
        else if (impl::format::data == format)
        {
            const auto data = nmos::get_video_smpte291_parameters(sdp_params);

            const nmos::rational grain_rate = data.exactframerate;

            source = nmos::make_data_source(source_id, device_id, clock, grain_rate, settings);
            flow = nmos::make_sdianc_data_flow(flow_id, source_id, device_id, data.did_sdids, settings);
            flow.data[nmos::fields::grain_rate] = nmos::make_rational(grain_rate);
        }
        else if (impl::format::mux == format)
        {
            const auto mux = nmos::get_video_SMPTE2022_6_parameters(sdp_params);

            // hmm, this should take account of sdp_params.framerate
            const nmos::rational grain_rate = nmos::rates::rate50;

            source = nmos::make_mux_source(source_id, device_id, clock, grain_rate, settings);
            flow = nmos::make_mux_flow(flow_id, source_id, device_id, settings);
            flow.data[nmos::fields::grain_rate] = nmos::make_rational(grain_rate);
        }

        const auto manifest_href = nmos::experimental::make_manifest_api_manifest(sender_id, settings);
        auto sender = nmos::make_sender(sender_id, flow_id, nmos::transports::rtp, device_id, manifest_href.to_string(), interface_names, settings);
        if (impl::format::video == format)
        {
            if (nmos::media_types::video_jxsv == media_type)
            {
                const auto video = nmos::get_video_jxsv_parameters(sdp_params);

                // additional attributes required by BCP-006-01
                // see https://specs.amwa.tv/bcp-006-01/releases/v1.0.0/docs/NMOS_With_JPEG_XS.html#senders

                const auto transport_bit_rate = impl::get_transport_bit_rate(sdp_params);
                if (0 != transport_bit_rate)
                {
                    sender.data[nmos::fields::bit_rate] = value(transport_bit_rate);
                }
                const auto packet_transmission_mode = nmos::parse_packet_transmission_mode(video.packetmode, video.transmode);
                if (nmos::packet_transmission_modes::codestream != packet_transmission_mode)
                {
                    sender.data[nmos::fields::packet_transmission_mode] = value(packet_transmission_mode.name);
                }
                if (!video.tp.empty())
                {
                    sender.data[nmos::fields::st2110_21_sender_type] = value(video.tp.name);
                }
            }
        }

        auto connection_sender = nmos::make_connection_rtp_sender(sender_id, transport_params.size() > 1);
        // add some constraints; these should be completed fully!
        auto& constraints = connection_sender.data[nmos::fields::endpoint_constraints];
        for (int leg = 0; leg < (int)constraints.size(); ++leg)
        {
            const auto& source_ip = nmos::fields::source_ip(transport_params.at(leg));
            if (!source_ip.is_string()) throw std::invalid_argument("Missing x-nvnmos-iface-ip or source-filter attribute in SDP");
            constraints[leg][nmos::fields::source_ip] = value_of({
                { nmos::fields::constraint_enum, value_of({ source_ip }) }
            });
        }
        impl::resolve_auto(sender, connection_sender, connection_sender.data[nmos::fields::endpoint_active][nmos::fields::transport_params], utility::s2us(sdp_));

        // override default label and description from model.settings
        sender.data[nmos::fields::label] = value::string(sdp_params.session_name);
        sender.data[nmos::fields::description] = value::string(session_info);
        // set the name as a resource tag
        impl::set_name(sender, name);
        // set the group hint as a resource tag
        if (!group_hint.empty()) impl::set_group_hint(sender, group_hint);

        impl::insert_resource(node_resources, std::move(source), gate);
        impl::insert_resource(node_resources, std::move(flow), gate);
        impl::insert_resource(node_resources, std::move(sender), gate);
        impl::insert_resource(connection_resources, std::move(connection_sender), gate);

        {
            const auto urls = impl::make_resource_api_urls(settings, sender_id, nmos::types::sender);
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Created " << std::make_pair(sender_id, nmos::types::sender) << " (" << name << "): " << urls.first << " " << urls.second;
        }

        // update node's interfaces

        const auto interfaces_for_bindings = !interfaces.empty()
            ? boost::copy_range<std::map<utility::string_t, nmos::node_interface>>(interfaces | boost::adaptors::transformed([](const nmos::node_interface& interface)
            {
                return std::make_pair(interface.name, interface);
            }))
            : impl::get_interfaces_for_bindings(interface_names, host_interfaces);
        impl::update_node_interfaces(node_resources, node_id, { {}, interface_names }, interfaces_for_bindings, settings);

        // update node's clocks

        auto& clock_settings = nvnmos::fields::clocks(settings)[clock.name];
        auto ptp_domain = nmos::fields::ptp_domain_number(clock_settings);
        impl::update_node_clock(node_resources, node_id, impl::make_node_clock(clock, ts_refclks, ptp_domain));

        clock_settings[nmos::fields::ptp_domain_number] = ptp_domain;

        // insert into settings

        nvnmos::fields::senders(settings)[sender_id] = impl::make_transport_settings(nmos::transports::rtp, utility::s2us(sdp_));
    }

    void node_implementation_add_rtp_receiver_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& sdp_, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto sdp = sdp::parse_session_description(sdp_);
        const auto sdp_params = nmos::get_session_description_sdp_parameters(sdp);
        const auto transport_params = impl::get_session_description_transport_params(nmos::types::receiver, sdp);
        const auto name = impl::get_session_description_resource_name(sdp);
        // hm, could check the name is unique across all receivers
        const auto group_hint = impl::get_session_description_group_hint(sdp);
        const auto session_info = impl::get_session_description_session_info(sdp);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto receiver_id = impl::make_id(seed_id, nmos::types::receiver, name);

        const auto media_type = nmos::get_media_type(sdp_params);
        const auto format = impl::get_format(media_type);
        const auto want_caps = !impl::has_session_description_caps(sdp);

        const auto interfaces = impl::get_session_description_interfaces(sdp, transport_params.size());
        const auto interface_names = boost::copy_range<std::vector<utility::string_t>>(
            boost::irange(0, (int)transport_params.size()) | boost::adaptors::transformed([&](const int& leg)
            {
                if (!interfaces.empty()) return interfaces.at(leg).name;
                return impl::get_interface_name(nmos::types::receiver, transport_params.at(leg), host_interfaces);
            }));

        nmos::resource receiver;

        if (impl::format::video == format)
        {
            receiver = nmos::make_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, nmos::formats::video, { media_type }, settings);

            if (want_caps)
            {
                if (nmos::media_types::video_raw == media_type)
                {
                    const auto video = nmos::get_video_raw_parameters(sdp_params);

                    const auto interlace_modes = video.interlace
                        ? std::vector<utility::string_t>{ nmos::interlace_modes::interlaced_bff.name, nmos::interlace_modes::interlaced_tff.name, nmos::interlace_modes::interlaced_psf.name }
                        : std::vector<utility::string_t>{ nmos::interlace_modes::progressive.name };
                    receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                        value_of({
                            { nmos::caps::format::grain_rate, nmos::make_caps_rational_constraint({ video.exactframerate }) },
                            { nmos::caps::format::frame_width, nmos::make_caps_integer_constraint({ video.width }) },
                            { nmos::caps::format::frame_height, nmos::make_caps_integer_constraint({ video.height }) },
                            { nmos::caps::format::interlace_mode, nmos::make_caps_string_constraint(interlace_modes) },
                            { nmos::caps::format::color_sampling, nmos::make_caps_string_constraint({ video.sampling.name }) }
                        })
                    });
                }
                else if (nmos::media_types::video_jxsv == media_type)
                {
                    const auto video = nmos::get_video_jxsv_parameters(sdp_params);

                    // some of the parameter constraints recommended by BCP-006-01
                    // could also include common video ones (grain_rate, frame_width, frame_height, etc.)
                    // see https://specs.amwa.tv/bcp-006-01/releases/v1.0.0/docs/NMOS_With_JPEG_XS.html#receivers
                    const auto format_bit_rate = impl::get_format_bit_rate(sdp_params);
                    const auto transport_bit_rate = impl::get_transport_bit_rate(sdp_params);
                    const auto packet_transmission_mode = nmos::parse_packet_transmission_mode(video.packetmode, video.transmode);
                    receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                        value_of({
                            // hm, could enumerate lower profiles, levels or sublevels?
                            { !video.profile.empty() ? nmos::caps::format::profile.key : U(""), nmos::make_caps_string_constraint({ video.profile.name }) },
                            { !video.level.empty() ? nmos::caps::format::level.key : U(""), nmos::make_caps_string_constraint({ video.level.name }) },
                            { !video.sublevel.empty() ? nmos::caps::format::sublevel.key : U(""), nmos::make_caps_string_constraint({ video.sublevel.name }) },
                            { 0 != format_bit_rate ? nmos::caps::format::bit_rate.key : U(""), nmos::make_caps_integer_constraint({}, nmos::no_minimum<int64_t>(), (int64_t)format_bit_rate) },
                            { 0 != transport_bit_rate ? nmos::caps::transport::bit_rate.key : U(""), nmos::make_caps_integer_constraint({}, nmos::no_minimum<int64_t>(), (int64_t)transport_bit_rate) },
                            { nmos::caps::transport::packet_transmission_mode, nmos::make_caps_string_constraint({ packet_transmission_mode.name }) }
                        })
                    });
                }
                receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
            }
        }
        else if (impl::format::audio == format)
        {
            const auto audio = nmos::get_audio_L_parameters(sdp_params);

            receiver = nmos::make_audio_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, audio.bit_depth, settings);
            if (want_caps)
            {
                receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                    value_of({
                        { nmos::caps::format::channel_count, nmos::make_caps_integer_constraint({ audio.channel_count }) },
                        { nmos::caps::format::sample_rate, nmos::make_caps_rational_constraint({ audio.sample_rate }) },
                        { nmos::caps::format::sample_depth, nmos::make_caps_integer_constraint({ audio.bit_depth }) },
                        { 0 != sdp_params.packet_time ? nmos::caps::transport::packet_time.key : U(""), nmos::make_caps_number_constraint({ sdp_params.packet_time }) },
                        { 0 != sdp_params.max_packet_time ? nmos::caps::transport::max_packet_time.key : U(""), nmos::make_caps_number_constraint({ sdp_params.max_packet_time }) }
                    })
                });
                receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
            }
        }
        else if (impl::format::data == format)
        {
            const auto data = nmos::get_video_smpte291_parameters(sdp_params);

            receiver = nmos::make_sdianc_data_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, settings);
            if (want_caps)
            {
                if (data.exactframerate)
                {
                    receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                        value_of({
                            { nmos::caps::format::grain_rate, nmos::make_caps_rational_constraint({ data.exactframerate }) }
                        })
                    });
                    receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
                }
            }
        }
        else if (impl::format::mux == format)
        {
            const auto mux = nmos::get_video_SMPTE2022_6_parameters(sdp_params);

            receiver = nmos::make_mux_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, settings);
            if (want_caps)
            {
                // hmm, add a constraint set, e.g. taking account of sdp_params.framerate
            }
        }

        auto connection_receiver = nmos::make_connection_rtp_receiver(receiver_id, transport_params.size() > 1);
        // add some constraints; these should be completed fully!
        auto& constraints = connection_receiver.data[nmos::fields::endpoint_constraints];
        for (int leg = 0; leg < (int)constraints.size(); ++leg)
        {
            constraints[leg][nmos::fields::interface_ip] = value_of({
                { nmos::fields::constraint_enum, value_of({ nmos::fields::interface_ip(transport_params.at(leg)) }) }
            });
        }

        impl::resolve_auto(receiver, connection_receiver, connection_receiver.data[nmos::fields::endpoint_active][nmos::fields::transport_params]);

        // override default label and description from settings
        receiver.data[nmos::fields::label] = value::string(sdp_params.session_name);
        receiver.data[nmos::fields::description] = value::string(session_info);
        // set the name as a resource tag
        impl::set_name(receiver, name);
        // set the group hint as a resource tag
        if (!group_hint.empty()) impl::set_group_hint(receiver, group_hint);

        impl::insert_resource(node_resources, std::move(receiver), gate);
        impl::insert_resource(connection_resources, std::move(connection_receiver), gate);

        {
            const auto urls = impl::make_resource_api_urls(settings, receiver_id, nmos::types::receiver);
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Created " << std::make_pair(receiver_id, nmos::types::receiver) << " (" << name << "): " << urls.first << " " << urls.second;
        }

        // update node's interfaces

        const auto interfaces_for_bindings = !interfaces.empty()
            ? boost::copy_range<std::map<utility::string_t, nmos::node_interface>>(interfaces | boost::adaptors::transformed([](const nmos::node_interface& interface)
            {
                return std::make_pair(interface.name, interface);
            }))
            : impl::get_interfaces_for_bindings(interface_names, host_interfaces);
        impl::update_node_interfaces(node_resources, node_id, { {}, interface_names }, interfaces_for_bindings, settings);

        // insert into settings

        nvnmos::fields::receivers(settings)[receiver_id] = impl::make_transport_settings(nmos::transports::rtp, utility::s2us(sdp_));
    }

    void node_implementation_add_mxl_sender_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& flow_def_, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto flow_def = impl::parse_mxl_flow_def(flow_def_);
        const auto name = impl::get_mxl_flow_def_name(flow_def);
        // hm, could check the name is unique across all senders
        const auto group_hint = impl::get_mxl_flow_def_group_hint(flow_def);
        const auto mxl_domain_id = impl::get_mxl_flow_def_domain_id(flow_def);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto source_id = impl::make_id(seed_id, nmos::types::source, name);
        const auto flow_id = impl::make_id(seed_id, nmos::types::flow, name);
        const auto sender_id = impl::make_id(seed_id, nmos::types::sender, name);

        // the NMOS Flow id is always generated (flow_id, above); the MXL flow id is taken from the flow
        // definition's 'id' property if present, otherwise it falls back to the NMOS Flow id
        const auto explicit_mxl_flow_id = impl::get_mxl_flow_def_id(flow_def);
        const auto mxl_flow_id = !explicit_mxl_flow_id.empty() ? explicit_mxl_flow_id : flow_id;

        // for now, only manage a single clock (internal); MXL has no equivalent to RTP ts-refclk,
        // that will require a new nvnmos extension property
        const auto clock = nmos::clock_names::clk0;

        const nmos::media_type media_type{ nmos::fields::media_type(flow_def) };
        const auto format = impl::get_format(media_type);

        nmos::resource source;
        nmos::resource flow;

        if (impl::format::video == format)
        {
            // assuming an MXL flow definition follows IS-04 v1.3 once an MXL SDK schema lands,
            // the structural properties (grain_rate, frame_width, frame_height, colorspace) are
            // required; let nmos::fields::* propagate json_exception if absent
            const auto grain_rate = nmos::parse_rational(nmos::fields::grain_rate(flow_def));
            const auto frame_width = nmos::fields::frame_width(flow_def);
            const auto frame_height = nmos::fields::frame_height(flow_def);
            const auto colorspace = nmos::colorspace{ nmos::fields::colorspace(flow_def) };
            // interlace_mode and transfer_characteristic have defaults; absent in MXL flow_def
            // maps to absent in NMOS Flow
            const auto interlace_mode = nmos::interlace_mode{ nmos::fields::interlace_mode(flow_def) };
            const auto transfer_characteristic = nmos::transfer_characteristic{ nmos::fields::transfer_characteristic(flow_def) };

            // components is required (carries sampling and bit depth); use nmos-cpp's helper
            // to classify the sampling and let it throw on unsupported components
            const auto& components = nmos::fields::components(flow_def);
            const auto sampling = nmos::details::make_sampling(components);
            const auto bit_depth = nmos::fields::bit_depth(components.at(0));

            source = nmos::make_video_source(source_id, device_id, clock, grain_rate, settings);
            flow = nmos::make_coded_video_flow(
                flow_id, source_id, device_id,
                grain_rate, frame_width, frame_height, interlace_mode,
                colorspace, transfer_characteristic, sampling, bit_depth,
                media_type,
                settings
            );
        }
        else if (impl::format::audio == format)
        {
            // sample_rate, channel_count and bit_depth are required; let the accessors propagate
            // json_exception if absent
            const auto sample_rate = nmos::parse_rational(nmos::fields::sample_rate(flow_def));
            const auto channel_count = nvnmos::fields::channel_count(flow_def);
            const auto bit_depth = nmos::fields::bit_depth(flow_def);

            const auto channels = boost::copy_range<std::vector<nmos::channel>>(
                boost::irange(0, channel_count) | boost::adaptors::transformed([&](const int& index)
            {
                return nmos::channel{ U(""), nmos::channel_symbols::Undefined(1 + index) };
            }));

            source = nmos::make_audio_source(source_id, device_id, clock, sample_rate, channels, settings);
            flow = nmos::make_raw_audio_flow(flow_id, source_id, device_id, sample_rate, bit_depth, settings);
            flow.data[nmos::fields::media_type] = value::string(media_type.name);
        }
        else if (impl::format::data == format)
        {
            const auto grain_rate = nmos::parse_rational(nmos::fields::grain_rate(flow_def));

            source = nmos::make_data_source(source_id, device_id, clock, grain_rate, settings);
            flow = nmos::make_sdianc_data_flow(flow_id, source_id, device_id, {}, settings);
            flow.data[nmos::fields::grain_rate] = nmos::make_rational(grain_rate);
        }
        else // e.g. if (impl::format::mux == format)
        {
            throw std::invalid_argument("Unsupported media type for MXL sender: " + utility::us2s(media_type.name));
        }

        // MXL sender: no manifest_href (no /transportfile), no interface_bindings
        auto sender = nmos::make_sender(sender_id, flow_id, nmos::transports::mxl, device_id, {}, {}, settings);

        auto connection_sender = nmos::make_connection_mxl_sender(sender_id, mxl_domain_id, mxl_flow_id);

        impl::resolve_auto(sender, connection_sender, connection_sender.data[nmos::fields::endpoint_active][nmos::fields::transport_params]);

        // label and description are required (per IS-04 v1.3); let the accessors propagate
        // json_exception if absent
        sender.data[nmos::fields::label] = value::string(nmos::fields::label(flow_def));
        sender.data[nmos::fields::description] = value::string(nmos::fields::description(flow_def));
        impl::set_name(sender, name);
        if (!group_hint.empty()) impl::set_group_hint(sender, group_hint);

        impl::insert_resource(node_resources, std::move(source), gate);
        impl::insert_resource(node_resources, std::move(flow), gate);
        impl::insert_resource(node_resources, std::move(sender), gate);
        impl::insert_resource(connection_resources, std::move(connection_sender), gate);

        {
            const auto urls = impl::make_resource_api_urls(settings, sender_id, nmos::types::sender);
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Created " << std::make_pair(sender_id, nmos::types::sender) << " (" << name << "): " << urls.first << " " << urls.second;
        }

        // insert into settings

        nvnmos::fields::senders(settings)[sender_id] = impl::make_transport_settings(nmos::transports::mxl, utility::s2us(flow_def_));
    }

    void node_implementation_add_mxl_receiver_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& flow_def_, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto flow_def = impl::parse_mxl_flow_def(flow_def_);
        const auto name = impl::get_mxl_flow_def_name(flow_def);
        // hm, could check the name is unique across all receivers
        const auto group_hint = impl::get_mxl_flow_def_group_hint(flow_def);
        const auto mxl_domain_id = impl::get_mxl_flow_def_domain_id(flow_def);
        const auto want_caps = !impl::has_mxl_flow_def_caps(flow_def);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto receiver_id = impl::make_id(seed_id, nmos::types::receiver, name);

        const nmos::media_type media_type{ nmos::fields::media_type(flow_def) };
        const auto format = impl::get_format(media_type);

        nmos::resource receiver;

        if (impl::format::video == format)
        {
            receiver = nmos::make_receiver(receiver_id, device_id, nmos::transports::mxl, {}, nmos::formats::video, { media_type }, settings);

            if (want_caps)
            {
                // structural properties are required; let nmos::fields::* throw if absent
                const auto grain_rate = nmos::parse_rational(nmos::fields::grain_rate(flow_def));
                const auto frame_width = nmos::fields::frame_width(flow_def);
                const auto frame_height = nmos::fields::frame_height(flow_def);
                const auto& components = nmos::fields::components(flow_def);
                const auto sampling = nmos::details::make_sampling(components);
                const auto interlace_mode_name = nmos::fields::interlace_mode(flow_def);
                const auto interlace_modes = nmos::interlace_modes::progressive.name == interlace_mode_name
                    ? std::vector<utility::string_t>{ nmos::interlace_modes::progressive.name }
                    : std::vector<utility::string_t>{ nmos::interlace_modes::interlaced_bff.name, nmos::interlace_modes::interlaced_tff.name, nmos::interlace_modes::interlaced_psf.name };

                receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                    value_of({
                        { nmos::caps::format::media_type, nmos::make_caps_string_constraint({ media_type.name }) },
                        { nmos::caps::format::grain_rate, nmos::make_caps_rational_constraint({ grain_rate }) },
                        { nmos::caps::format::frame_width, nmos::make_caps_integer_constraint({ frame_width }) },
                        { nmos::caps::format::frame_height, nmos::make_caps_integer_constraint({ frame_height }) },
                        { !interlace_mode_name.empty() ? nmos::caps::format::interlace_mode.key : U(""), nmos::make_caps_string_constraint(interlace_modes) },
                        { nmos::caps::format::color_sampling, nmos::make_caps_string_constraint({ sampling.name }) }
                    })
                });
                receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
            }
        }
        else if (impl::format::audio == format)
        {
            receiver = nmos::make_receiver(receiver_id, device_id, nmos::transports::mxl, {}, nmos::formats::audio, { media_type }, settings);

            if (want_caps)
            {
                // sample_rate, channel_count and bit_depth are required; let nmos::fields::* throw if absent
                const auto sample_rate = nmos::parse_rational(nmos::fields::sample_rate(flow_def));
                const auto channel_count = nvnmos::fields::channel_count(flow_def);
                const auto bit_depth = nmos::fields::bit_depth(flow_def);

                receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                    value_of({
                        { nmos::caps::format::media_type, nmos::make_caps_string_constraint({ media_type.name }) },
                        { nmos::caps::format::channel_count, nmos::make_caps_integer_constraint({ channel_count }) },
                        { nmos::caps::format::sample_rate, nmos::make_caps_rational_constraint({ sample_rate }) },
                        { nmos::caps::format::sample_depth, nmos::make_caps_integer_constraint({ bit_depth }) }
                    })
                });
                receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
            }
        }
        else if (impl::format::data == format)
        {
            receiver = nmos::make_sdianc_data_receiver(receiver_id, device_id, nmos::transports::mxl, {}, settings);

            if (want_caps)
            {
                if (flow_def.has_field(nmos::fields::grain_rate))
                {
                    const auto grain_rate = nmos::parse_rational(nmos::fields::grain_rate(flow_def));
                    receiver.data[nmos::fields::caps][nmos::fields::constraint_sets] = value_of({
                        value_of({
                            { nmos::caps::format::grain_rate, nmos::make_caps_rational_constraint({ grain_rate }) }
                        })
                    });
                    receiver.data[nmos::fields::version] = receiver.data[nmos::fields::caps][nmos::fields::version] = value(nmos::make_version());
                }
            }
        }
        else // e.g. if (impl::format::mux == format)
        {
            throw std::invalid_argument("Unsupported media type for MXL receiver: " + utility::us2s(media_type.name));
        }

        auto connection_receiver = nmos::make_connection_mxl_receiver(receiver_id, mxl_domain_id);

        impl::resolve_auto(receiver, connection_receiver, connection_receiver.data[nmos::fields::endpoint_active][nmos::fields::transport_params]);

        // label and description are required (per IS-04 v1.3); let the accessors propagate
        // json_exception if absent
        receiver.data[nmos::fields::label] = value::string(nmos::fields::label(flow_def));
        receiver.data[nmos::fields::description] = value::string(nmos::fields::description(flow_def));
        impl::set_name(receiver, name);
        if (!group_hint.empty()) impl::set_group_hint(receiver, group_hint);

        impl::insert_resource(node_resources, std::move(receiver), gate);
        impl::insert_resource(connection_resources, std::move(connection_receiver), gate);

        {
            const auto urls = impl::make_resource_api_urls(settings, receiver_id, nmos::types::receiver);
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Created " << std::make_pair(receiver_id, nmos::types::receiver) << " (" << name << "): " << urls.first << " " << urls.second;
        }

        // insert into settings

        nvnmos::fields::receivers(settings)[receiver_id] = impl::make_transport_settings(nmos::transports::mxl, utility::s2us(flow_def_));
    }

    void node_implementation_remove_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const nmos::type& type, const nvnmos::name& name, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        // find sender or receiver with specified name

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto id = impl::make_id(seed_id, type, name);
        const std::pair<nmos::id, nmos::type> id_type{ id, type };
        auto resource = nmos::find_resource(node_resources, id_type);

        if (node_resources.end() != resource)
        {
            const auto bindings_removed = boost::copy_range<std::vector<utility::string_t>>(
                nmos::fields::interface_bindings(resource->data) | boost::adaptors::transformed([](const web::json::value& binding)
            {
                return binding.as_string();
            }));

            // erase connection resource

            nmos::erase_resource(connection_resources, id);

            // erase node resources (sender before flow before source)

            nmos::id device_id = nmos::fields::device_id(resource->data);

            nmos::id flow_id;
            nmos::id source_id;

            if (nmos::types::sender == resource->type)
            {
                // cf. impl::find_source_for_sender
                const auto& flow_id_or_null = nmos::fields::flow_id(resource->data);
                if (!flow_id_or_null.is_null())
                {
                    flow_id = flow_id_or_null.as_string();

                    auto flow = nmos::find_resource(node_resources, { flow_id, nmos::types::flow });
                    if (node_resources.end() != flow)
                    {
                        source_id = nmos::fields::source_id(flow->data);
                    }
                }
            }

            nmos::erase_resource(node_resources, id);
            if (!flow_id.empty()) nmos::erase_resource(node_resources, flow_id);
            if (!source_id.empty()) nmos::erase_resource(node_resources, source_id);

            // update node's interfaces

            impl::update_node_interfaces(node_resources, node_id, { bindings_removed, {} }, {}, settings);

            // erase from settings

            auto& configs = nmos::types::sender == type ? nvnmos::fields::senders(settings) : nvnmos::fields::receivers(settings);
            if (configs.has_field(id))
            {
                configs.erase(id);
            }

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Destroyed " << id_type << " (" << name << ")";
        }
        else
        {
            throw std::invalid_argument("Could not find " + utility::us2s(type.name) + ": " + utility::us2s(id) + " (" + utility::us2s(name) + ")");
        }
    }

    // This constructs and inserts a node resource and a device resource into the model, based on the model settings.
    void node_implementation_init(nmos::node_model& model, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_init_(model.node_resources, host_interfaces, model.settings, gate);

        model.notify();
    }

    // This constructs and inserts sources/flows/senders into the model, based on the specified transport file.
    void node_implementation_add_sender(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        if (nmos::transports::rtp == nmos::transport_base(transport))
        {
            node_implementation_add_rtp_sender_(model.node_resources, model.connection_resources, transport_file, host_interfaces, model.settings, gate);
        }
        else if (nmos::transports::mxl == nmos::transport_base(transport))
        {
            node_implementation_add_mxl_sender_(model.node_resources, model.connection_resources, transport_file, model.settings, gate);
        }
        else
        {
            throw std::invalid_argument("Unsupported transport: " + utility::us2s(transport.name));
        }

        model.notify();
    }

    // This constructs and inserts a receiver into the model, based on the specified transport file.
    void node_implementation_add_receiver(nmos::node_model& model, const nmos::transport& transport, const std::string& transport_file, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        if (nmos::transports::rtp == nmos::transport_base(transport))
        {
            node_implementation_add_rtp_receiver_(model.node_resources, model.connection_resources, transport_file, host_interfaces, model.settings, gate);
        }
        else if (nmos::transports::mxl == nmos::transport_base(transport))
        {
            node_implementation_add_mxl_receiver_(model.node_resources, model.connection_resources, transport_file, model.settings, gate);
        }
        else
        {
            throw std::invalid_argument("Unsupported transport: " + utility::us2s(transport.name));
        }

        model.notify();
    }

    // This removes sources/flows/senders from the model corresponding to the specified name.
    void node_implementation_remove_sender(nmos::node_model& model, const nvnmos::name& sender_name, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_remove_connection_(model.node_resources, model.connection_resources, nmos::types::sender, sender_name, host_interfaces, model.settings, gate);

        model.notify();
    }

    // This removes the receiver from the model corresponding to the specified name.
    void node_implementation_remove_receiver(nmos::node_model& model, const nvnmos::name& receiver_name, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_remove_connection_(model.node_resources, model.connection_resources, nmos::types::receiver, receiver_name, host_interfaces, model.settings, gate);

        model.notify();
    }

    // System API node behaviour callback to perform application-specific operations when the global configuration resource changes
    nmos::system_global_handler make_node_implementation_system_global_handler(nmos::node_model& model, slog::base_gate& gate)
    {
        // this example uses the callback to update the settings
        // (an 'empty' std::function disables System API node behaviour)
        return [&](const web::uri& system_uri, const web::json::value& system_global)
        {
            if (!system_uri.is_empty())
            {
                slog::log<slog::severities::info>(gate, SLOG_FLF) << "New system global configuration discovered from the System API at: " << system_uri.to_string();

                // although this example immediately updates the settings, the effect is not propagated
                // in either Registration API behaviour or the senders' /transportfile endpoints until
                // an update to these is forced by other circumstances

                auto system_global_settings = nmos::parse_system_global_data(system_global).second;
                web::json::merge_patch(model.settings, system_global_settings, true);
            }
            else
            {
                slog::log<slog::severities::warning>(gate, SLOG_FLF) << "System global configuration is not discoverable";
            }
        };
    }

    // Registration API node behaviour callback to perform application-specific operations when the current Registration API changes
    nmos::registration_handler make_node_implementation_registration_handler(slog::base_gate& gate)
    {
        return [&](const web::uri& registration_uri)
        {
            if (!registration_uri.is_empty())
            {
                slog::log<slog::severities::info>(gate, SLOG_FLF) << "Started registered operation with Registration API at: " << registration_uri.to_string();
            }
            else
            {
                slog::log<slog::severities::warning>(gate, SLOG_FLF) << "Stopped registered operation";
            }
        };
    }

    // Connection API callback to parse "transport_file" during a PATCH /staged request
    nmos::transport_file_parser make_node_implementation_transport_file_parser()
    {
        // this uses a custom transport file parser to handle video/jxsv in addition to the core media types
        // otherwise, it could simply return &nmos::parse_rtp_transport_file
        // (if this callback is specified, an 'empty' std::function is not allowed)
        return [](const nmos::resource& receiver, const nmos::resource& connection_receiver, const utility::string_t& transport_file_type, const utility::string_t& transport_file_data, slog::base_gate& gate)
        {
            const auto validate_sdp_parameters = [](const web::json::value& receiver, const nmos::sdp_parameters& sdp_params)
            {
                if (nmos::media_types::video_jxsv == nmos::get_media_type(sdp_params))
                {
                    nmos::validate_video_jxsv_sdp_parameters(receiver, sdp_params);
                }
                else
                {
                    // validate core media types, i.e., "video/raw", "audio/L", "video/smpte291" and "video/SMPTE2022-6"
                    nmos::validate_sdp_parameters(receiver, sdp_params);
                }
            };
            return nmos::details::parse_rtp_transport_file(validate_sdp_parameters, receiver, connection_receiver, transport_file_type, transport_file_data, gate);
        };
    }

    // Connection API callback to perform application-specific validation of the merged /staged endpoint during a PATCH /staged request
    nmos::details::connection_resource_patch_validator make_node_implementation_patch_validator()
    {
        // if a transport file hasn't been staged, assume default values based on the original SDP data used to configure the receiver
        // so this callback does not need to do any validation beyond what is expressed by the schemas and /constraints endpoint
        return {};
    }

    namespace impl
    {
        void insert_resource(nmos::resources& resources, nmos::resource&& resource, slog::base_gate& gate)
        {
            const std::pair<nmos::id, nmos::type> id_type{ resource.id, resource.type };
            if (!nmos::insert_resource(resources, std::move(resource)).second)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Model update error: " << id_type;
                throw node_implementation_exception();
            }

            // per-resource progress reporting
            slog::log<slog::severities::more_info>(gate, SLOG_FLF) << "Updated model with " << id_type;
        }

        void resolve_auto(const nmos::resource& resource, const nmos::resource& connection_resource, web::json::value& transport_params, const utility::string_t& transport_file)
        {
            using web::json::value;

            const std::pair<nmos::id, nmos::type> id_type{ connection_resource.id, connection_resource.type };
            // this code relies on the specific constraints added by node_implementation_init
            const auto& constraints = nmos::fields::endpoint_constraints(connection_resource.data);

            const auto transport_base = nmos::transport_base(nmos::transport{ nmos::fields::transport(resource.data) });
            const auto is_rtp = nmos::transports::rtp == transport_base;
            const auto is_mxl = nmos::transports::mxl == transport_base;

            // "In some cases the behaviour is more complex, and may be determined by the vendor."
            // See https://specs.amwa.tv/is-05/releases/v1.0.0/docs/2.2._APIs_-_Server_Side_Implementation.html#use-of-auto
            if (nmos::types::sender == id_type.second && is_rtp)
            {
                const auto sdp = sdp::parse_session_description(utility::us2s(transport_file));
                const auto auto_params = impl::get_session_description_transport_params(nmos::types::sender, sdp);
                for (int leg = 0; leg < (int)constraints.size(); ++leg)
                {
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::source_ip, [&] { return web::json::front(nmos::fields::constraint_enum(constraints.at(leg).at(nmos::fields::source_ip))); });
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::destination_ip, [&] { return U("auto") != nmos::fields::destination_ip(auto_params.at(leg)).as_string() ? nmos::fields::destination_ip(auto_params.at(leg)) : value::string(make_source_specific_multicast_address_v4(id_type.first, leg)); });
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::destination_port, [&] { return nmos::fields::destination_port(auto_params.at(leg)).is_integer() ? nmos::fields::destination_port(auto_params.at(leg)) : value(5004); });
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::source_port, [&] { return nmos::fields::source_port(auto_params.at(leg)).is_integer() ? nmos::fields::source_port(auto_params.at(leg)) : value(5004); });
                }
                // lastly, apply the specification defaults for any properties not handled above
                nmos::resolve_rtp_auto(id_type.second, transport_params);
            }
            else if (nmos::types::receiver == id_type.second && is_rtp)
            {
                for (int leg = 0; leg < (int)constraints.size(); ++leg)
                {
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::interface_ip, [&] { return web::json::front(nmos::fields::constraint_enum(constraints.at(leg).at(nmos::fields::interface_ip))); });
                }
                // lastly, apply the specification defaults for any properties not handled above
                nmos::resolve_rtp_auto(id_type.second, transport_params);
            }
            else if (nmos::types::sender == id_type.second && is_mxl)
            {
                // BCP-007-03: MXL has a single transport leg per sender
                nmos::details::resolve_auto(transport_params[0], nmos::fields::mxl_domain_id, [&] { return impl::resolve_mxl_domain_id(constraints.at(0).at(nmos::fields::mxl_domain_id)); });
                nmos::details::resolve_auto(transport_params[0], nmos::fields::mxl_flow_id, [&] { return web::json::front(nmos::fields::constraint_enum(constraints.at(0).at(nmos::fields::mxl_flow_id))); });
            }
            else if (nmos::types::receiver == id_type.second && is_mxl)
            {
                // BCP-007-03: MXL has a single transport leg per receiver, and mxl_flow_id does not use "auto" (UUID or null only)
                nmos::details::resolve_auto(transport_params[0], nmos::fields::mxl_domain_id, [&] { return impl::resolve_mxl_domain_id(constraints.at(0).at(nmos::fields::mxl_domain_id)); });
            }
        }
    }

    // Connection API activation callback to resolve "auto" values when /staged is transitioned to /active
    nmos::connection_resource_auto_resolver make_node_implementation_auto_resolver(const nmos::settings& settings)
    {
        using web::json::value;

        return [&settings](const nmos::resource& resource, const nmos::resource& connection_resource, value& transport_params)
        {
            auto& configs = nmos::types::sender == resource.type ? nvnmos::fields::senders(settings) : nvnmos::fields::receivers(settings);
            auto config = configs.as_object().find(resource.id);
            if (configs.as_object().end() == config) return;
            impl::resolve_auto(resource, connection_resource, transport_params, nvnmos::fields::transport_file(config->second));
        };
    }

    // Connection API activation callback to update senders' /transportfile endpoint - captures node_resources and settings by reference!
    nmos::connection_sender_transportfile_setter make_node_implementation_transportfile_setter(const nmos::resources& node_resources, const nmos::settings& settings)
    {
        using web::json::value;

        // as part of activation, the sender /transportfile should be updated based on the active transport parameters
        return [&node_resources, &settings](const nmos::resource& sender, const nmos::resource& connection_sender, value& endpoint_transportfile)
        {
            auto& configs = nvnmos::fields::senders(settings);
            auto config = configs.as_object().find(sender.id);

            const auto is_rtp = nmos::transports::rtp == nmos::transport_base(nmos::transport{ nmos::fields::transport(sender.data) });

            if (configs.as_object().end() != config && is_rtp)
            {
                const auto& sdp_data = nvnmos::fields::transport_file(config->second);

                const auto parsed_sdp = sdp::parse_session_description(utility::us2s(sdp_data));

                auto sdp_params = nmos::get_session_description_sdp_parameters(parsed_sdp);

                // remove custom nvnmos parameters
                sdp_params.fmtp.erase(std::remove_if(sdp_params.fmtp.begin(), sdp_params.fmtp.end(), [](const nmos::sdp_parameters::fmtp_t::value_type& param)
                {
                    return boost::algorithm::starts_with(param.first, U("x-nvnmos-"));
                }), sdp_params.fmtp.end());

                // update ts-refclk based on current clock
                {
                    const auto seed_id = nmos::experimental::fields::seed_id(settings);
                    const auto node_id = impl::make_id(seed_id, nmos::types::node);

                    auto node = nmos::find_resource(node_resources, { node_id, nmos::types::node });
                    if (node_resources.end() == node) throw node_implementation_exception();

                    auto source = impl::find_source_for_sender(node_resources, sender);
                    if (node_resources.end() == source) throw node_implementation_exception();

                    auto& clock_or_null = nmos::fields::clock_name(source->data);
                    if (clock_or_null.is_null()) throw node_implementation_exception();
                    const auto clock = nmos::clock_name(clock_or_null.as_string());
                    const auto ptp_domain = nmos::fields::ptp_domain_number(web::json::field_as_value_or{ clock.name, {} }(nvnmos::fields::clocks(settings)));

                    sdp_params.ts_refclk = nmos::details::make_ts_refclk(node->data, source->data, sender.data, ptp_domain);
                }

                // update session version since the resulting /transportfile isn't necessarily identical to the original SDP data
                // (stream-format through utility::ostringstreamed because origin.session_version is now a utility::string_t;
                // a bare uint64_t would silently truncate to a single non-digit char via std::string::operator=(char),
                // which then trips sdp::parse on the round-trip with "expected a sequence of digits at line 2")
                sdp_params.origin.session_version = utility::ostringstreamed(sdp::ntp_now() >> 32);

                auto& transport_params = nmos::fields::transport_params(nmos::fields::endpoint_active(connection_sender.data));

                // use nmos::make_session_description rather than impl::make_session_description for /transportfile
                // because e.g. the custom SDP attributes in nvnmos::attributes are only for 'internal' use
                auto session_description = nmos::make_session_description(sdp_params, transport_params);
                auto sdp = utility::s2us(sdp::make_session_description(session_description));
                endpoint_transportfile = nmos::make_connection_rtp_sender_transportfile(sdp);
            }
        };
    }

    // Connection API activation callback to perform application-specific operations to complete activation
    nmos::connection_activation_handler make_node_implementation_connection_activation_handler(connection_activation_handler connection_activated, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_from_elements;

        return [&settings, connection_activated, &gate](const nmos::resource& resource, const nmos::resource& connection_resource)
        {
            const std::pair<nmos::id, nmos::type> id_type{ resource.id, resource.type };
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Activating " << id_type;

            auto& configs = nmos::types::sender == resource.type ? nvnmos::fields::senders(settings) : nvnmos::fields::receivers(settings);
            auto config = configs.as_object().find(resource.id);

            if (configs.as_object().end() == config) return;

            const auto transport_base = nmos::transport_base(nmos::transport{ nmos::fields::transport(resource.data) });
            const auto is_rtp = nmos::transports::rtp == transport_base;
            const auto is_mxl = nmos::transports::mxl == transport_base;

            if (!is_rtp && !is_mxl) return;

            const auto name = impl::get_name(resource);

            const auto& endpoint_active = nmos::fields::endpoint_active(connection_resource.data);

            // determine the new state of the sender or receiver
            const bool active = nmos::fields::master_enable(endpoint_active);

            if (active)
            {
                if (is_rtp)
                {
                    // get the active transport file from the sender's /transportfile endpoint or receiver's /active transport_file object
                    auto& transportfile = nmos::types::sender == id_type.second
                        ? nmos::fields::endpoint_transportfile(connection_resource.data)
                        : nmos::fields::transport_file(endpoint_active);
                    auto& transportfile_data_or_null = nmos::fields::transportfile_data(transportfile);

                    // if a transport file hasn't been staged to a receiver, or a sender hasn't been activated, assume default values
                    // based on the original SDP data used to configure the receiver or sender
                    const auto& transportfile_data = !transportfile_data_or_null.is_null() && !transportfile_data_or_null.as_string().empty()
                        ? transportfile_data_or_null.as_string()
                        : nvnmos::fields::transport_file(config->second);

                    // activate the sender or receiver with the effective SDP file for the /active transport_params

                    auto& transport_params = nmos::fields::transport_params(endpoint_active);

                    const auto parsed_sdp = sdp::parse_session_description(utility::us2s(transportfile_data));
                    auto sdp_params = nmos::get_session_description_sdp_parameters(parsed_sdp);

                    if (transport_params.size() > 1)
                    {
                        // A single-legged SDP file applied to a two-legged Receiver, configures it to receive on the primary interface by default.
                        // By setting rtp_enabled to false for the first leg and rtp_enabled to true, and setting all the other transport params
                        // for the second leg, a client can configure the Receiver on the secondary interface (for example because that interface
                        // is the one on the same network as the single-legged Sender).
                        // It is therefore also possible for a client to apply a single-legged SDP file but set rtp_enabled to true on both legs.
                        // This seems pretty pointless but can be accommodated by manipulating the sdp_params...
                        sdp_params.group.semantics = sdp::group_semantics::duplication;
                        if (sdp_params.group.media_stream_ids.size() < transport_params.size())
                        {
                            sdp_params.group.media_stream_ids = boost::copy_range<std::vector<utility::string_t>>(
                                boost::irange(0, (int)transport_params.size()) | boost::adaptors::transformed([&](const int& index)
                                {
                                    return utility::ostringstreamed(index);
                                })
                            );
                        }
                        if (!sdp_params.ts_refclk.empty())
                        {
                            // passing a "self referencing" value is OK
                            // see https://cplusplus.github.io/LWG/issue679
                            sdp_params.ts_refclk.resize(transport_params.size(), sdp_params.ts_refclk.front());
                        }
                    }

                    // update session version since the resulting SDP data isn't necessarily identical to the original
                    // sender's /transportfile (e.g. due to rtp_enabled) or receiver's /active transport_file object
                    sdp_params.origin.session_version = utility::ostringstreamed(sdp::ntp_now() >> 32);

                    const auto group_hint = impl::get_group_hint(resource);
                    const auto& session_info = nmos::fields::description(resource.data);
                    const auto caps = nmos::types::receiver == id_type.second && impl::has_no_receiver_caps(resource.data);
                    auto merged_sdp = impl::make_session_description(id_type.second, name, group_hint, session_info, sdp_params, transport_params, caps);
                    const auto sdp_data = sdp::make_session_description(merged_sdp);

                    connection_activated(id_type.second, name, sdp_data);
                }
                else if (is_mxl)
                {
                    // BCP-007-03: MXL transport_params is a single-leg array
                    const auto& active_leg = nmos::fields::transport_params(endpoint_active).at(0);
                    const auto& mxl_flow_id_or_null = nmos::fields::mxl_flow_id(active_leg);

                    // for receivers, mxl_flow_id may be null even when master_enable is true.
                    // unlike RTP's rtp_enabled=false (where the other transport params may still
                    // be useful), there's nothing actionable for the application without a
                    // concrete flow id, so translate this to a deactivation callback.
                    if (mxl_flow_id_or_null.is_null())
                    {
                        connection_activated(id_type.second, name, {});
                        return;
                    }

                    // hmm, we do not have access to the MXL flow definition of the flow here;
                    // for now, splice the active mxl_domain_id, mxl_flow_id transport parameters
                    // into the original MXL flow definition JSON
                    // IS-05 uses null for an application-resolved mxl_domain_id; the flow_def tag
                    // convention is an empty string, so normalise here
                    const auto& mxl_domain_id_or_null = nmos::fields::mxl_domain_id(active_leg);
                    const auto mxl_domain_id = !mxl_domain_id_or_null.is_null() ? mxl_domain_id_or_null.as_string() : utility::string_t{};
                    const auto& mxl_flow_id = mxl_flow_id_or_null.as_string();

                    const auto& config_flow_def_data = nvnmos::fields::transport_file(config->second);
                    auto config_flow_def = web::json::value::parse(config_flow_def_data);
                    const auto flow_def_data = impl::make_mxl_flow_def(std::move(config_flow_def), mxl_domain_id, mxl_flow_id);

                    connection_activated(id_type.second, name, flow_def_data);
                }
            }
            else
            {
                // deactivate sender or receiver
                connection_activated(id_type.second, name, {});
            }
        };
    }

    void node_implementation_activate_rtp_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const nmos::resource& resource, const std::string& sdp, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto set_transportfile = make_node_implementation_transportfile_setter(node_resources, settings);

        const std::pair<nmos::id, nmos::type> id_type{ resource.id, resource.type };

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);

        if (nmos::types::sender == id_type.second && !sdp.empty())
        {
            auto source = impl::find_source_for_sender(node_resources, resource);
            if (node_resources.end() == source)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Could not find source for " << id_type;
                throw node_implementation_exception();
            }
            auto& clock_or_null = nmos::fields::clock_name(source->data);
            if (clock_or_null.is_null())
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Source " << source->id << " for " << id_type << " has no clock";
                throw node_implementation_exception();
            }
            const auto clock = nmos::clock_name(clock_or_null.as_string());

            // hmm, the IS-05 update already calls sdp::parse_session_description(sdp) twice...
            const auto parsed_sdp = sdp::parse_session_description(sdp);
            const auto ts_refclks = impl::get_session_description_ts_refclks(parsed_sdp);

            auto& clock_settings = nvnmos::fields::clocks(settings)[clock.name];
            auto ptp_domain = nmos::fields::ptp_domain_number(clock_settings);
            impl::update_node_clock(node_resources, node_id, impl::make_node_clock(clock, ts_refclks, ptp_domain));

            clock_settings[nmos::fields::ptp_domain_number] = ptp_domain;
        }

        const auto activation_time = nmos::tai_now();

        nmos::modify_resource(connection_resources, id_type.first, [&](nmos::resource& connection_resource)
        {
            const auto at = value::string(nmos::make_version(activation_time));

            connection_resource.data[nmos::fields::version] = at;

            // Update the IS-05 resource's /active endpoint

            auto& active = connection_resource.data[nmos::fields::endpoint_active];

            active[nmos::types::sender == connection_resource.type ? nmos::fields::receiver_id : nmos::fields::sender_id] = value::null();
            active[nmos::fields::master_enable] = value::boolean(!sdp.empty());
            active[nmos::fields::activation] = nmos::make_activation();

            if (!sdp.empty())
            {
                if (nmos::types::receiver == connection_resource.type)
                {
                    active[nmos::fields::transport_file] = value_of({
                        { nmos::fields::data, utility::s2us(sdp) },
                        { nmos::fields::type, nmos::media_types::application_sdp.name }
                    });
                }

                active[nmos::fields::transport_params] = impl::get_session_description_transport_params(connection_resource.type, sdp::parse_session_description(sdp));
            }

            // Update an IS-05 sender's /transportfile endpoint

            if (nmos::types::sender == id_type.second)
            {
                set_transportfile(resource, connection_resource, connection_resource.data[nmos::fields::endpoint_transportfile]);
            }
        });

        nmos::modify_resource(node_resources, id_type.first, [&](nmos::resource& res)
        {
            nmos::set_resource_subscription(res, !sdp.empty(), {}, activation_time);
        });
    }

    void node_implementation_activate_mxl_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const nmos::resource& resource, const std::string& flow_def, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const std::pair<nmos::id, nmos::type> id_type{ resource.id, resource.type };

        const auto activation_time = nmos::tai_now();

        // BCP-007-03: MXL connections do not have a transport file in the IS-05 sense;
        // the supplied flow_def is the application's MXL flow definition for the new active state.
        // From it, derive the active mxl_domain_id and mxl_flow_id transport parameters.
        const auto parsed_flow_def = flow_def.empty() ? web::json::value{} : web::json::value::parse(flow_def);

        nmos::modify_resource(connection_resources, id_type.first, [&](nmos::resource& connection_resource)
        {
            const auto at = value::string(nmos::make_version(activation_time));

            connection_resource.data[nmos::fields::version] = at;

            auto& active = connection_resource.data[nmos::fields::endpoint_active];

            active[nmos::types::sender == connection_resource.type ? nmos::fields::receiver_id : nmos::fields::sender_id] = value::null();
            active[nmos::fields::master_enable] = value::boolean(!flow_def.empty());
            active[nmos::fields::activation] = nmos::make_activation();

            if (!flow_def.empty())
            {
                // MXL connections have no transport file in the IS-05 sense; for receivers,
                // endpoint_active.transport_file is initialised to { data: null, type: null }
                // at resource creation and stays that way; for senders, there is no /transportfile

                const auto mxl_domain_id = impl::get_mxl_flow_def_domain_id(parsed_flow_def);
                const auto mxl_flow_id = impl::get_mxl_flow_def_id(parsed_flow_def);
                // nvnmos always associates an NMOS flow with every sender, and requires receivers
                // to bind to a concrete MXL flow at activation time; to unbind, the application
                // should deactivate (pass an empty transport_file) instead
                if (mxl_flow_id.empty()) throw std::invalid_argument("Missing id property in MXL flow definition for activation");

                // IS-05 uses null for an application-resolved mxl_domain_id; the flow_def tag
                // convention is an empty string, so normalise here
                active[nmos::fields::transport_params] = value_of({
                    value_of({
                        { nmos::fields::mxl_domain_id, !mxl_domain_id.empty() ? value::string(mxl_domain_id) : value::null() },
                        { nmos::fields::mxl_flow_id, mxl_flow_id }
                    })
                });
            }
        });

        nmos::modify_resource(node_resources, id_type.first, [&](nmos::resource& res)
        {
            nmos::set_resource_subscription(res, !flow_def.empty(), {}, activation_time);
        });
    }

    void node_implementation_activate_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const nmos::type& type, const nvnmos::name& name, const std::string& transport_file, nmos::settings& settings, slog::base_gate& gate)
    {
        // find the sender or receiver with the specified name; a Sender and a
        // Receiver are permitted to share a name, so we pick by `type`.

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto resource_id = impl::make_id(seed_id, type, name);
        const std::pair<nmos::id, nmos::type> id_type{ resource_id, type };

        auto resource = nmos::find_resource(node_resources, id_type);

        if (node_resources.end() == resource) throw std::invalid_argument("Could not find " + utility::us2s(type.name) + ": " + utility::us2s(resource_id) + " (" + utility::us2s(name) + ")");

        // hmm, consider how to handle this 'internal' activation
        // * for now, setting /active endpoint directly, cf. nmos::connection_activation_thread
        // * alternatively, by setting or patching /staged with an immediate or scheduled activation

        slog::log<slog::severities::info>(gate, SLOG_FLF) << "Updating " << id_type << " (" << name << ")";

        const auto transport_base = nmos::transport_base(nmos::transport{ nmos::fields::transport(resource->data) });

        if (nmos::transports::rtp == transport_base)
        {
            node_implementation_activate_rtp_connection_(node_resources, connection_resources, *resource, transport_file, settings, gate);
        }
        else if (nmos::transports::mxl == transport_base)
        {
            node_implementation_activate_mxl_connection_(node_resources, connection_resources, *resource, transport_file, settings, gate);
        }
        else
        {
            slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Unsupported transport for " << id_type << " (" << name << "): " << transport_base.name;
            throw node_implementation_exception();
        }
    }

    // This updates the transport parameters and transport file for the specified sender or receiver based on the specified transport file.
    // `type` selects between a sender and a receiver with the same `name` on the Node.
    // For now, the transport file is not validated against the existing sender or receiver capabilities and constraints.
    // Does not invoke the application's connection activation callback.
    void node_implementation_activate_connection(nmos::node_model& model, const nmos::type& type, const nvnmos::name& name, const std::string& transport_file, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        node_implementation_activate_connection_(model.node_resources, model.connection_resources, type, name, transport_file, model.settings, gate);

        model.notify();
    }

    namespace impl
    {
        // like nmos::make_session_description for 'internal' use
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value make_session_description(const nmos::type& type, const nvnmos::name& name, const utility::string_t& group_hint, const utility::string_t& session_info, const nmos::sdp_parameters& sdp_params, const web::json::value& transport_params, bool caps)
        {
            using web::json::value;

            auto session_description = nmos::make_session_description(sdp_params, transport_params);

            {
                // using op[] rather than at because there can be no session-level attributes
                auto& session_attributes = session_description[sdp::fields::attributes];
                web::json::push_back(session_attributes, sdp::named_value(nvnmos::attributes::name, name));
                if (!group_hint.empty()) web::json::push_back(session_attributes, sdp::named_value(nvnmos::attributes::group_hint, group_hint));

                if (!session_info.empty())
                {
                    session_description[sdp::fields::information] = value::string(session_info);
                }
            }

            auto& media_descriptions = session_description[sdp::fields::media_descriptions];
            for (int leg = 0; leg < (int)transport_params.size(); ++leg)
            {
                const auto& transport_param = transport_params.at(leg);

                auto& media_description = media_descriptions.at(leg);
                auto& media_attributes = media_description.at(sdp::fields::attributes);

                if (nmos::types::receiver == type)
                {
                    if (caps)
                    {
                        web::json::push_back(media_attributes, sdp::named_value(nvnmos::attributes::caps, utility::ostringstreamed(sdp_params.rtpmap.payload_type)));
                    }
                }
                else // if (nmos::types::sender == type)
                {
                    const auto& source_port = nmos::fields::source_port(transport_param);
                    if (source_port.is_integer())
                    {
                        web::json::push_back(media_attributes, sdp::named_value(nvnmos::attributes::source_port, utility::ostringstreamed(source_port.as_integer())));
                    }
                }

                const auto& interface_ip = nmos::types::receiver == type ? nmos::fields::interface_ip : nmos::fields::source_ip;
                const auto& address = interface_ip(transport_param).as_string();
                web::json::push_back(media_attributes, sdp::named_value(nvnmos::attributes::interface_ip, address));

                // include an 'a=inactive' attribute line in media descriptions for legs where rtp_enabled is false
                if (!nmos::fields::rtp_enabled(transport_param))
                {
                    web::json::push_back(media_attributes, sdp::named_value(sdp::attributes::inactive));
                }
            }

            return session_description;
        }

        nmos::sdp_parameters::ts_refclk_t parse_ts_refclk(const web::json::value& value)
        {
            sdp::ts_refclk_source clock_source{ sdp::fields::clock_source(value) };
            if (sdp::ts_refclk_sources::ptp == clock_source)
            {
                // no ptp-server implies traceable
                return nmos::sdp_parameters::ts_refclk_t::ptp(sdp::ptp_version{ sdp::fields::ptp_version(value) }, sdp::fields::ptp_server(value));
            }
            else if (sdp::ts_refclk_sources::local_mac == clock_source)
            {
                return nmos::sdp_parameters::ts_refclk_t::local_mac(sdp::fields::mac_address(value));
            }
            return {};
        }

        std::vector<nmos::sdp_parameters::ts_refclk_t> parse_ts_refclks(const web::json::value& attributes)
        {
            using web::json::value;

            return boost::copy_range<std::vector<nmos::sdp_parameters::ts_refclk_t>>(attributes.as_array() | boost::adaptors::filtered([](const value& nv)
            {
                return sdp::fields::name(nv) == sdp::attributes::ts_refclk;
            }) | boost::adaptors::transformed([](const value& nv)
            {
                return parse_ts_refclk(sdp::fields::value(nv));
            }) | boost::adaptors::filtered([](const nmos::sdp_parameters::ts_refclk_t& ts_refclk)
            {
                return !ts_refclk.clock_source.empty();
            }));
        }

        // like nmos::get_session_description_sdp_parameters
        // with support for multiple ts-refclk attributes in each media description
        std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>> get_session_description_ts_refclks(const web::json::value& session_description)
        {
            using web::json::value;

            const auto& media_descriptions = sdp::fields::media_descriptions(session_description);
            return boost::copy_range<std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>>>(media_descriptions.as_array()
                | boost::adaptors::transformed([&session_description](const value& media_description)
            {
                auto ts_refclks = impl::parse_ts_refclks(sdp::fields::attributes(media_description));

                // default to the "session-level" value if no "media-level" value
                if (ts_refclks.empty())
                {
                    ts_refclks = impl::parse_ts_refclks(sdp::fields::attributes(session_description));
                }

                return ts_refclks;
            }));
        }

        // like nmos::get_session_description_transport_params
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value get_session_description_transport_params(const nmos::type& type, const web::json::value& session_description)
        {
            using web::json::value;

            auto transport_params = nmos::get_session_description_transport_params(session_description);

            const auto& media_descriptions = sdp::fields::media_descriptions(session_description);
            for (int leg = 0; leg < (int)transport_params.size(); ++leg)
            {
                auto& transport_param = transport_params.at(leg);

                if (nmos::types::sender == type)
                {
                    // use multicast_ip if set, otherwise interface_ip if set and not "0.0.0.0", otherwise "auto"
                    const auto& multicast_ip = nmos::fields::multicast_ip(transport_param);
                    const auto& interface_ip = nmos::fields::interface_ip(transport_param);
                    auto destination_ip = !multicast_ip.is_null()
                        ? multicast_ip
                        : interface_ip.is_string() && U("0.0.0.0") != interface_ip.as_string() ? interface_ip : value(U("auto"));
                    transport_param[nmos::fields::destination_ip] = std::move(destination_ip);
                    transport_param.erase(nmos::fields::multicast_ip);
                    transport_param.erase(nmos::fields::interface_ip);

                    if (nmos::fields::destination_port(transport_param).is_integer() && 0 == nmos::fields::destination_port(transport_param).as_integer())
                    {
                        transport_param[nmos::fields::destination_port] = value(U("auto"));
                    }

                    // hm, source port is unknown unless the custom SDP attribute is present...
                    // in the /active endpoint this could be indicated by unresolved "auto" or zero?
                    transport_param[nmos::fields::source_port] = value(U("auto"));
                }

                const auto& media_description = media_descriptions.at(leg);
                const auto& media_attributes = sdp::fields::attributes(media_description);
                const auto& ma = media_attributes.as_array();

                if (nmos::types::sender == type)
                {
                    auto interface_ip = sdp::find_name(ma, nvnmos::attributes::interface_ip);
                    if (ma.end() != interface_ip)
                    {
                        transport_param[nmos::fields::source_ip] = sdp::fields::value(*interface_ip);
                    }

                    auto source_port = sdp::find_name(ma, nvnmos::attributes::source_port);
                    if (ma.end() != source_port)
                    {
                        transport_param[nmos::fields::source_port] = value(utility::istringstreamed(sdp::fields::value(*source_port).as_string(), 0));
                    }
                    // else leave "auto"
                }
                else // if (nmos::types::receiver == type)
                {
                    auto interface_ip = sdp::find_name(ma, nvnmos::attributes::interface_ip);
                    if (ma.end() != interface_ip)
                    {
                        transport_param[nmos::fields::interface_ip] = sdp::fields::value(*interface_ip);
                    }
                }

                // set rtp_enabled to false in legs for media descriptions which include an 'a=inactive' attribute line
                auto inactive = sdp::find_name(ma, sdp::attributes::inactive);
                if (ma.end() != inactive)
                {
                    transport_param[nmos::fields::rtp_enabled] = value::boolean(false);
                }
            }

            return transport_params;
        }

        // get the (required) NvNmos resource name from the `x-nvnmos-name` custom attribute (not the SDP `s=` session-name line); throws std::invalid_argument if absent or empty
        nvnmos::name get_session_description_resource_name(const web::json::value& session_description)
        {
            const auto& session_attributes = sdp::fields::attributes(session_description);
            {
                const auto& sa = session_attributes.as_array();

                auto name = sdp::find_name(sa, nvnmos::attributes::name);
                if (sa.end() != name)
                {
                    const auto& value = sdp::fields::value(*name).as_string();
                    if (!value.empty()) return value;
                }
            }

            throw std::invalid_argument("Missing or empty x-nvnmos-name attribute in SDP");
        }

        // get the optional group hint from the custom attribute
        utility::string_t get_session_description_group_hint(const web::json::value& session_description)
        {
            const auto& session_attributes = sdp::fields::attributes(session_description);
            {
                const auto& sa = session_attributes.as_array();

                auto group_hint = sdp::find_name(sa, nvnmos::attributes::group_hint);
                if (sa.end() != group_hint)
                {
                    return sdp::fields::value(*group_hint).as_string();
                }
            }

            return U("");
        }

        // get the optional session information
        utility::string_t get_session_description_session_info(const web::json::value& session_description)
        {
            return sdp::fields::information(session_description);
        }

        // get the optional capabilities from the custom attribute
        // a=x-nvnmos-caps:<format> <format specific parameter constraints>
        // where <format> is the RTP payload type and <format specific parameter constraints>
        // is zero or more parameters to be constrained, overriding the "a=fmtp:" line
        // for now, just using this to indicate whether constraint sets are wanted at all
        bool has_session_description_caps(const web::json::value& session_description)
        {
            using web::json::value;

            const auto& media_descriptions = sdp::fields::media_descriptions(session_description);
            // hm, for simplicity, read caps only from the first media description
            if (web::json::empty(media_descriptions)) return false;
            const auto& media_description = media_descriptions.at(0);
            const auto& media_attributes = sdp::fields::attributes(media_description);
            {
                const auto& ma = media_attributes.as_array();

                auto caps = sdp::find_name(ma, nvnmos::attributes::caps);
                return ma.end() != caps;
            }
        }

        // whether the IS-04 receiver has no BCP-004-01 constraint_sets (unconstrained)
        bool has_no_receiver_caps(const web::json::value& receiver)
        {
            return !nmos::fields::constraint_sets(nmos::fields::caps(receiver)).is_array();
        }

        // approximate IP/UDP/RTP overhead
        const auto transport_bit_rate_factor = 1.05;

        // get the format bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_format_bit_rate(const nmos::sdp_parameters& sdp_params)
        {
            // use custom format bit rate parameter if present
            const auto format_bit_rate = nmos::details::find_fmtp(sdp_params.fmtp, nvnmos::fields::format_bit_rate);
            if (sdp_params.fmtp.end() != format_bit_rate) return utility::istringstreamed<uint64_t>(format_bit_rate->second);
            // otherwise, calculate an approximate value based on custom transport bit rate parameter or bandwidth line
            const auto transport_bit_rate = nmos::details::find_fmtp(sdp_params.fmtp, nvnmos::fields::transport_bit_rate);
            if (sdp_params.fmtp.end() != transport_bit_rate) return uint64_t(utility::istringstreamed<uint64_t>(transport_bit_rate->second) / impl::transport_bit_rate_factor);
            if (sdp::bandwidth_types::application_specific == sdp_params.bandwidth.bandwidth_type) return uint64_t(sdp_params.bandwidth.bandwidth / impl::transport_bit_rate_factor);
            return 0;
        }

        // get the transport bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_transport_bit_rate(const nmos::sdp_parameters& sdp_params)
        {
            // use custom transport bit rate parameter if present
            const auto transport_bit_rate = nmos::details::find_fmtp(sdp_params.fmtp, nvnmos::fields::transport_bit_rate);
            if (sdp_params.fmtp.end() != transport_bit_rate) return utility::istringstreamed<uint64_t>(transport_bit_rate->second);
            // otherwise, calculate an approximate value based on custom format bit rate parameter if present
            const auto format_bit_rate = nmos::details::find_fmtp(sdp_params.fmtp, nvnmos::fields::format_bit_rate);
            // round to nearest Megabit/second per examples in VSF TR-08:2022
            if (sdp_params.fmtp.end() != format_bit_rate) return uint64_t(utility::istringstreamed<uint64_t>(format_bit_rate->second) * impl::transport_bit_rate_factor / 1e3 + 0.5) * 1000;
            // or fall back to bandwidth line
            if (sdp::bandwidth_types::application_specific == sdp_params.bandwidth.bandwidth_type) return sdp_params.bandwidth.bandwidth;
            return 0;
        }

        // identify supported format from media type
        format get_format(const nmos::media_type& media_type)
        {
            // ST 2110 media types
            if (nmos::media_types::video_raw == media_type) return format::video;
            if (nmos::media_types::video_jxsv == media_type) return format::video;
            if (nmos::media_types::audio_L(24) == media_type) return format::audio;
            if (nmos::media_types::audio_L(16) == media_type) return format::audio;
            if (nmos::media_types::video_smpte291 == media_type) return format::data;
            if (nmos::media_types::video_SMPTE2022_6 == media_type) return format::mux;
            // MXL media types
            if (nmos::media_type{ U("video/v210") } == media_type) return format::video;
            if (nmos::media_type{ U("video/v210a") } == media_type) return format::video;
            if (nmos::media_type{ U("audio/float32") } == media_type) return format::audio;
            throw std::invalid_argument("Unsupported media type: " + utility::us2s(media_type.name));
        }

        // get a little mnemonic string to use in resource labels and descriptions
        utility::string_t get_format_hint(format format)
        {
            switch (format)
            {
            case format::video: return U("v");
            case format::audio: return U("a");
            case format::data: return U("d");
            case format::mux: return U("m");
            }
            throw node_implementation_exception{};
        }

        // find interface with the specified address
        std::vector<web::hosts::experimental::host_interface>::const_iterator find_interface(const std::vector<web::hosts::experimental::host_interface>& interfaces, const utility::string_t& address)
        {
            return boost::range::find_if(interfaces, [&](const web::hosts::experimental::host_interface& interface)
            {
                return interface.addresses.end() != boost::range::find(interface.addresses, address);
            });
        }

        // validate a MAC address (hyphen or colon separators, any hex case) and format as IS-04 port_id
        utility::string_t normalize_mac_address(const utility::string_t& value)
        {
            static const utility::regex_t pattern(U("^(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}$"), bst::regex_constants::icase);

            if (!bst::regex_match(value, pattern))
            {
                throw std::invalid_argument("Invalid x-nvnmos-iface <port-id>: " + utility::us2s(value));
            }

            return boost::algorithm::to_lower_copy(boost::replace_all_copy(value, U(":"), U("-")));
        }

        nmos::node_interface parse_iface(const utility::string_t& value)
        {
            // <name> <port-id>
            // <name> <chassis-id> <port-id>
            // <name> <port-id> <attached-chassis-id> <attached-port-id>
            // <name> <chassis-id> <port-id> <attached-chassis-id> <attached-port-id>

            std::vector<utility::string_t> tokens;
            boost::split(tokens, value, boost::is_any_of(U(" ")), boost::token_compress_on);
            if (2 > tokens.size() || tokens.size() > 5)
            {
                throw std::invalid_argument("Invalid x-nvnmos-iface: " + utility::us2s(value) + ", expected: <interface-name> [<chassis-id>] <port-id> [<attached-chassis-id> <attached-port-id>]");
            }

            const auto has_chassis = 1 == (tokens.size() % 2);
            const auto port_index = has_chassis ? 2 : 1;
            const auto has_attached = tokens.size() >= 4;
            return {
                has_chassis ? tokens[1] : U(""),
                normalize_mac_address(tokens[port_index]),
                tokens[0],
                has_attached ? tokens[port_index + 1] : U(""),
                has_attached ? tokens[port_index + 2] : U("") // unnormalized even if MAC address
            };
        }

        utility::string_t make_iface(const nmos::node_interface& node_interface)
        {
            utility::ostringstream_t os;
            os << node_interface.name;
            if (!node_interface.chassis_id.empty()) os << U(' ') << node_interface.chassis_id;
            os << U(' ') << node_interface.port_id;
            if (!node_interface.attached_chassis_id.empty() && !node_interface.attached_port_id.empty())
            {
                os << U(' ') << node_interface.attached_chassis_id << U(' ') << node_interface.attached_port_id;
            }
            return os.str();
        }

        std::vector<nmos::node_interface> get_session_description_interfaces(const web::json::value& session_description, size_t legs)
        {
            // hm, for now only supporting ST 2022-7 Separate Destination Addresses, i.e., a media description per leg,
            // not ST 2022-7 Separate Source Addresses, i.e., a single media description with a=source-filter: with two
            // source addresses and two a=ssrc:<ssrc-id> x-nvnmos-iface: attributes

            const auto& media_descriptions = sdp::fields::media_descriptions(session_description);

            auto interfaces = boost::copy_range<std::vector<nmos::node_interface>>(
                media_descriptions.as_array()
                | boost::adaptors::filtered([&](const web::json::value& media_description)
                {
                    const auto& media_attributes = sdp::fields::attributes(media_description).as_array();
                    return media_attributes.end() != sdp::find_name(media_attributes, nvnmos::attributes::interface);
                })
                | boost::adaptors::transformed([&](const web::json::value& media_description)
                {
                    const auto& media_attributes = sdp::fields::attributes(media_description).as_array();
                    const auto iface = sdp::find_name(media_attributes, nvnmos::attributes::interface);
                    return parse_iface(sdp::fields::value(*iface).as_string());
                }));

            if (0 != legs && !interfaces.empty() && interfaces.size() != legs)
            {
                throw std::invalid_argument("Invalid x-nvnmos-iface: expected one per leg");
            }

            return interfaces;
        }

        // get node interfaces from host_interfaces for interface_bindings
        std::map<utility::string_t, nmos::node_interface> get_interfaces_for_bindings(const std::vector<utility::string_t>& interface_names, const std::vector<web::hosts::experimental::host_interface>& host_interfaces)
        {
            if (interface_names.empty()) return {};

            const auto interfaces = nmos::experimental::node_interfaces(host_interfaces);

            return boost::copy_range<std::map<utility::string_t, nmos::node_interface>>(interface_names | boost::adaptors::transformed([&](const utility::string_t& name)
            {
                const auto found = interfaces.find(name);
                return std::make_pair(name, interfaces.end() != found ? found->second : nmos::node_interface{ {}, {}, name, {}, {} });
            }));
        }

        // look up the interface name from a transport param address via host_interfaces
        utility::string_t get_interface_name(const nmos::type& type, const web::json::value& transport_param, const std::vector<web::hosts::experimental::host_interface>& host_interfaces)
        {
            const auto& address = (nmos::types::sender == type ? nmos::fields::source_ip : nmos::fields::interface_ip)(transport_param).as_string();
            const auto interface = find_interface(host_interfaces, address);
            if (host_interfaces.end() == interface)
            {
                throw std::invalid_argument("No network interface corresponding to the connection address: " + utility::us2s(address)
                    + " (provide a=x-nvnmos-iface with IS-04 interface metadata)");
            }
            return interface->name;
        }

        // make a transport settings entry for a sender/receiver
        web::json::value make_transport_settings(const nmos::transport& transport, const utility::string_t& transport_file)
        {
            using web::json::value_of;

            return value_of({
                { nvnmos::fields::transport, transport.name },
                { nvnmos::fields::transport_file, transport_file }
            });
        }

        // generate repeatable ids for the node's resources
        nmos::id make_id(const nmos::id& seed_id, const nmos::type& type, const nvnmos::name& name)
        {
            return nmos::make_repeatable_id(seed_id, U("/x-nmos/node/") + type.name + U('/') + name);
        }

        // generate URLs for the Node API and Connection API
        std::pair<utility::string_t, utility::string_t> make_api_base_urls(const nmos::settings& settings)
        {
            auto build = [&](int port, const utility::string_t& api, const nmos::api_version& version)
            {
                return web::uri_builder()
                    .set_scheme(nmos::http_scheme(settings))
                    .set_host(nmos::get_host(settings))
                    .set_port(port)
                    .set_path(U("/x-nmos/") + api + U('/') + nmos::make_api_version(version))
                    .to_uri()
                    .to_string();
            };
            return {
                build(nmos::fields::node_port(settings), U("node"), *nmos::is04_versions::from_settings(settings).rbegin()),
                build(nmos::fields::connection_port(settings), U("connection"), *nmos::is05_versions::from_settings(settings).rbegin())
            };
        }

        // generate URLs for a sender or receiver in the Node API and Connection API
        std::pair<utility::string_t, utility::string_t> make_resource_api_urls(const nmos::settings& settings, const nmos::id& id, const nmos::type& type)
        {
            const auto urls = make_api_base_urls(settings);
            const auto path = U('/') + type.name + U("s/") + id;
            return { urls.first + path, urls.second + U("/single") + path };
        }

        // generate a repeatable source-specific multicast address for each leg of a sender
        utility::string_t make_source_specific_multicast_address_v4(const nmos::id& id, int leg)
        {
            // hash the pseudo-random id and leg to generate the address
            const auto s = id + U('/') + utility::conversions::details::to_string_t(leg);
            const auto h = std::hash<utility::string_t>{}(s);
            auto a = boost::asio::ip::address_v4(uint32_t(h)).to_bytes();
            // ensure the address is in the source-specific multicast block reserved for local host allocation, 232.0.1.0-232.255.255.255
            // see https://www.iana.org/assignments/multicast-addresses/multicast-addresses.xhtml#multicast-addresses-10
            a[0] = 232;
            a[2] |= 1;
            return utility::s2us(boost::asio::ip::address_v4(a).to_string());
        }

        // set the name for the sender or receiver as a resource tag
        void set_name(nmos::resource& resource, const nvnmos::name& name)
        {
            using web::json::value_of;

            resource.data[nmos::fields::tags][nvnmos::fields::name] = value_of({ name });
        }

        // get the name for the sender or receiver from a resource tag
        nvnmos::name get_name(const nmos::resource& resource)
        {
            const auto& names = nvnmos::fields::name(resource.data.at(nmos::fields::tags)).as_array();
            return !web::json::empty(names)
                ? web::json::front(names).as_string()
                : U("");
        }

        // set the group hint for the sender or receiver as a resource tag
        void set_group_hint(nmos::resource& resource, const utility::string_t& group_hint)
        {
            using web::json::value_of;

            resource.data[nmos::fields::tags][nmos::fields::group_hint] = value_of({ group_hint });
        }

        // get the group hint for the sender or receiver from a resource tag
        utility::string_t get_group_hint(const nmos::resource& resource)
        {
            const auto& tags = resource.data.at(nmos::fields::tags);
            const auto& group_hints = nmos::fields::group_hint(tags).as_array();
            return !web::json::empty(group_hints)
                ? web::json::front(group_hints).as_string()
                : U("");
        }

        // find the source for the flow referenced by the source
        nmos::resources::const_iterator find_source_for_sender(const nmos::resources& resources, const nmos::resource& sender)
        {
            const auto& flow_id_or_null = nmos::fields::flow_id(sender.data);
            if (flow_id_or_null.is_null()) return resources.end();
            const auto& flow_id = flow_id_or_null.as_string();
            auto flow = nmos::find_resource(resources, { flow_id, nmos::types::flow });
            if (resources.end() == flow) return resources.end();
            const auto& source_id = nmos::fields::source_id(flow->data);
            return nmos::find_resource(resources, { source_id, nmos::types::source });
        }

        // make a node clock based on specified SDP ts-refclk attributes for each leg and get the PTP domain if present
        web::json::value make_node_clock(const nmos::clock_name& clock_name, const std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>>& ts_refclks, int& ptp_domain)
        {
            if (ts_refclks.empty())
            {
                return nmos::make_internal_clock(clock_name);
            }

            // for now, assume either all legs have the same PTP clock reference, or all legs have a localmac clock reference
            // so just use the first leg
            const auto& ts_refclk_ = ts_refclks.front();

            // unfortunately, RFC 7273 ts-refclk allows us to know that the clock source is traceable or what the GMID is, not both
            // a=ts-refclk:ptp=<ptp version>:<ptp gmid>[:<ptp domain>]
            // a=ts-refclk:ptp=<ptp version>:traceable

            // the second form is represented in ts_refclk_t by an empty ptp_server

            auto ts_refclk = std::find_if(ts_refclk_.begin(), ts_refclk_.end(), [](const nmos::sdp_parameters::ts_refclk_t& ts_refclk)
            {
                return sdp::ts_refclk_sources::ptp == ts_refclk.clock_source
                    && sdp::ptp_versions::IEEE1588_2008 == ts_refclk.ptp_version
                    && !ts_refclk.ptp_server.empty();
            });
            auto traceable = ts_refclk_.end() != std::find_if(ts_refclk_.begin(), ts_refclk_.end(), [](const nmos::sdp_parameters::ts_refclk_t& ts_refclk)
            {
                return sdp::ts_refclk_sources::ptp == ts_refclk.clock_source
                    && sdp::ptp_versions::IEEE1588_2008 == ts_refclk.ptp_version
                    && ts_refclk.ptp_server.empty();
            });

            if (ts_refclk_.end() == ts_refclk)
            {
                if (!traceable)
                {
                    return nmos::make_internal_clock(clock_name);
                }

                // see https://standards.ieee.org/wp-content/uploads/import/documents/tutorials/eui.pdf
                static const auto null_gmid = U("ff-ff-ff-ff-ff-ff-ff-ff");

                return nmos::make_ptp_clock(clock_name, true, null_gmid, true);
            }

            const auto colon = ts_refclk->ptp_server.find(U(':'));
            const auto gmid = boost::algorithm::to_lower_copy(ts_refclk->ptp_server.substr(0, colon));
            if (utility::string_t::npos != colon)
            {
                ptp_domain = utility::istringstreamed(ts_refclk->ptp_server.substr(colon + 1), ptp_domain);
            }

            return nmos::make_ptp_clock(clock_name, traceable, gmid, true);
        }

        // modify node resource if necessary to update specified clock, which must already exist
        void update_node_clock(nmos::resources& node_resources, const nmos::id& node_id, const web::json::value& clock_)
        {
            using web::json::value;

            auto node = nmos::find_resource(node_resources, { node_id, nmos::types::node });
            if (node_resources.end() == node) throw node_implementation_exception();

            auto& clocks = nmos::fields::clocks(node->data);
            auto clock = std::find_if(clocks.begin(), clocks.end(), [&](const value& clock)
            {
                return nmos::fields::name(clock_) == nmos::fields::name(clock);
            });
            if (clocks.end() == clock) throw node_implementation_exception();

            if (clock_ != *clock)
            {
                nmos::modify_resource(node_resources, node_id, [&clock_](nmos::resource& node)
                {
                    node.data[nmos::fields::version] = value::string(nmos::make_version());

                    auto& clocks = nmos::fields::clocks(node.data);
                    auto clock = std::find_if(clocks.begin(), clocks.end(), [&](const value& clock)
                    {
                        return nmos::fields::name(clock_) == nmos::fields::name(clock);
                    });
                    *clock = clock_;
                });
            }
        }

        // modify node resource interfaces incrementally, maintaining interface_bindings reference counts in settings
        void update_node_interfaces(nmos::resources& node_resources, const nmos::id& node_id, const interface_bindings_update& bindings_update, const std::map<utility::string_t, nmos::node_interface>& interfaces, nmos::settings& settings)
        {
            using web::json::value;

            if (bindings_update.added.empty() && bindings_update.removed.empty()) return;

            auto node = nmos::find_resource(node_resources, { node_id, nmos::types::node });
            if (node_resources.end() == node) throw node_implementation_exception();

            auto& ref_counts = nvnmos::fields::interface_bindings(settings);

            std::set<utility::string_t> interface_names_to_add;
            std::set<utility::string_t> interface_names_to_remove;

            for (const auto& name : bindings_update.removed)
            {
                const auto count = web::json::field_as_integer_or{ name, 0 }(ref_counts);
                if (count <= 1)
                {
                    interface_names_to_remove.insert(name);
                    ref_counts.erase(name);
                }
                else
                {
                    ref_counts[name] = value::number(count - 1);
                }
            }

            for (const auto& name : bindings_update.added)
            {
                const auto count = web::json::field_as_integer_or{ name, 0 }(ref_counts);
                if (0 == count) interface_names_to_add.insert(name);
                ref_counts[name] = value::number(count + 1);
            }

            if (interface_names_to_add.empty() && interface_names_to_remove.empty()) return;

            nmos::modify_resource(node_resources, node_id, [&](nmos::resource& node)
            {
                auto& node_interfaces = nmos::fields::interfaces(node.data);

                for (const auto& name : interface_names_to_remove)
                {
                    const auto found = std::find_if(node_interfaces.begin(), node_interfaces.end(), [&](const value& interface)
                    {
                        return nmos::fields::name(interface) == name;
                    });
                    if (node_interfaces.end() != found) node_interfaces.erase(found);
                }

                for (const auto& name : interface_names_to_add)
                {
                    const auto found = interfaces.find(name);
                    web::json::push_back(node_interfaces, interfaces.end() != found
                        ? nmos::make_node_interface(found->second)
                        : nmos::make_node_interface({ {}, {}, name, {}, {} }));
                }

                node.data[nmos::fields::version] = value::string(nmos::make_version());
            });
        }

        // parse an MXL flow definition (JSON) including nvnmos extensions
        // (urn:x-nvnmos:tag:* entries inside the `tags` property);
        // propagates web::json::json_exception on parse error
        web::json::value parse_mxl_flow_def(const std::string& flow_def)
        {
            return web::json::value::parse(utility::s2us(flow_def));
        }

        // extract the first string from the named tag in the flow definition's
        // `tags` property (or empty if absent)
        utility::string_t get_mxl_flow_def_tag(const web::json::value& flow_def, const web::json::field_as_value_or& tag_field)
        {
            if (!flow_def.has_object_field(nmos::fields::tags)) return {};
            const auto& values = tag_field(flow_def.at(nmos::fields::tags)).as_array();
            if (web::json::empty(values) || !web::json::front(values).is_string()) return {};
            return web::json::front(values).as_string();
        }

        // extract the (required) name from the urn:x-nvnmos:tag:name tag;
        // throws std::invalid_argument if absent or empty
        nvnmos::name get_mxl_flow_def_name(const web::json::value& flow_def)
        {
            const auto value = get_mxl_flow_def_tag(flow_def, nvnmos::fields::name);
            if (value.empty()) throw std::invalid_argument("Missing or empty urn:x-nvnmos:tag:name tag in MXL flow definition");
            return value;
        }

        // extract an optional group hint from the urn:x-nmos:tag:grouphint/v1.0 tag (or empty)
        utility::string_t get_mxl_flow_def_group_hint(const web::json::value& flow_def)
        {
            return get_mxl_flow_def_tag(flow_def, nmos::fields::group_hint);
        }

        // returns true if the urn:x-nvnmos:tag:caps tag asks for an unconstrained
        // receiver (format-derived capabilities omitted)
        bool has_mxl_flow_def_caps(const web::json::value& flow_def)
        {
            if (!flow_def.has_object_field(nmos::fields::tags)) return false;
            return !web::json::empty(nvnmos::fields::caps(flow_def.at(nmos::fields::tags)).as_array());
        }

        // extract the MXL domain id from the urn:x-nvnmos:tag:mxl-domain-id tag;
        // returns empty when application-resolved (tag absent, empty array, or empty string)
        utility::string_t get_mxl_flow_def_domain_id(const web::json::value& flow_def)
        {
            return get_mxl_flow_def_tag(flow_def, nvnmos::fields::mxl_domain_id);
        }

        // resolve IS-05 mxl_domain_id "auto" from constraint: enum front or null
        web::json::value resolve_mxl_domain_id(const web::json::value& constraint)
        {
            return constraint.has_field(nmos::fields::constraint_enum)
                ? web::json::front(nmos::fields::constraint_enum(constraint))
                : web::json::value::null();
        }

        // extract the top-level 'id' property (or empty)
        utility::string_t get_mxl_flow_def_id(const web::json::value& flow_def)
        {
            return flow_def.has_string_field(nmos::fields::id)
                ? nmos::fields::id(flow_def)
                : utility::string_t{};
        }

        // produce a flow definition JSON string with the active MXL transport parameters spliced in
        std::string make_mxl_flow_def(web::json::value flow_def, const utility::string_t& mxl_domain_id, const utility::string_t& mxl_flow_id)
        {
            using web::json::value;
            using web::json::value_of;

            if (!flow_def.has_object_field(nmos::fields::tags))
            {
                flow_def[nmos::fields::tags] = value::object();
            }
            flow_def[nmos::fields::tags][nvnmos::fields::mxl_domain_id]
                = value_of({ value::string(mxl_domain_id) });
            flow_def[nmos::fields::id] = value::string(mxl_flow_id);

            return utility::us2s(flow_def.serialize());
        }

        bool has_channelmapping_control(const web::json::value& controls)
        {
            for (const auto& entry : controls.as_array())
            {
                if (boost::starts_with(nmos::fields::type(entry), U("urn:x-nmos:control:cm-ctrl/"))) return true;
            }
            return false;
        }

        void add_channelmapping_controls(web::json::value& controls, const nmos::settings& settings)
        {
            // see nmos::make_device
            const auto hosts = nmos::get_hosts(settings);
            for (const auto& version : nmos::is08_versions::from_settings(settings))
            {
                auto channelmapping_uri = web::uri_builder()
                    .set_scheme(nmos::http_scheme(settings))
                    .set_port(nmos::fields::channelmapping_port(settings))
                    .set_path(U("/x-nmos/channelmapping/") + nmos::make_api_version(version));
                const auto type = U("urn:x-nmos:control:cm-ctrl/") + nmos::make_api_version(version);

                for (const auto& host : hosts)
                {
                    web::json::push_back(controls, web::json::value_of({
                        { U("href"), channelmapping_uri.set_host(host).to_uri().to_string() },
                        { nmos::fields::type, type },
                        { U("authorization"), nmos::experimental::fields::server_authorization(settings) }
                    }));
                }
            }
        }

        void remove_channelmapping_controls(web::json::value& controls)
        {
            auto filtered = web::json::value::array();
            for (const auto& entry : controls.as_array())
            {
                if (boost::starts_with(nmos::fields::type(entry), U("urn:x-nmos:control:cm-ctrl/"))) continue;
                web::json::push_back(filtered, entry);
            }
            controls = std::move(filtered);
        }

        bool channelmapping_ids_contains(const web::json::value& channelmapping_ids, const nmos::channelmapping_id& channelmapping_id)
        {
            for (const auto& id : channelmapping_ids.as_array())
            {
                if (id.as_string() == channelmapping_id) return true;
            }
            return false;
        }

        bool channelmapping_ids_contains(const web::json::value& mappings, const web::json::field_as_value& ids_field, const nmos::channelmapping_id& channelmapping_id)
        {
            for (const auto& entry : mappings.as_object())
            {
                if (channelmapping_ids_contains(ids_field(entry.second), channelmapping_id)) return true;
            }
            return false;
        }

        nvnmos::name name_for_output(const web::json::value& mappings, const nmos::channelmapping_id& output_id)
        {
            for (const auto& entry : mappings.as_object())
            {
                if (channelmapping_ids_contains(nvnmos::fields::channelmapping_outputs(entry.second), output_id))
                {
                    return entry.first;
                }
            }
            return {};
        }

        channelmapping_active_map parse_active_map_from_output(const nmos::resource& output)
        {
            const auto channel_count = nmos::fields::channels(nmos::fields::endpoint_io(output.data)).size();
            channelmapping_active_map entries(channel_count);

            const auto& map = nmos::fields::map(nmos::fields::endpoint_active(output.data));
            for (size_t index = 0; index < channel_count; ++index)
            {
                const web::json::field_as_value output_channel_field{ utility::ostringstreamed(index) };
                if (!map.has_field(output_channel_field)) continue; // hm, unexpected, throw?

                const auto& output_channel = output_channel_field(map);
                const auto& input_or_null = nmos::fields::input(output_channel);
                const auto& channel_index_or_null = nmos::fields::channel_index(output_channel);
                if (input_or_null.is_null() || channel_index_or_null.is_null()) continue;

                entries[index] = std::make_pair(input_or_null.as_string(), web::json::as<uint32_t>(channel_index_or_null));
            }
            return entries;
        }

        void maybe_add_device_channelmapping_control(nmos::node_model& model, slog::base_gate& gate)
        {
            if (model.channelmapping_resources.empty()) return;

            const auto seed_id = nmos::experimental::fields::seed_id(model.settings);
            const auto device_id = impl::make_id(seed_id, nmos::types::device);

            const std::pair<nmos::id, nmos::type> id_type{ device_id, nmos::types::device };
            const auto device = nmos::find_resource(model.node_resources, id_type);
            if (model.node_resources.end() == device)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Could not find " << id_type;
                throw node_implementation_exception();
            }
            if (has_channelmapping_control(device->data.at(U("controls")))) return;

            nmos::modify_resource(model.node_resources, device_id, [&](nmos::resource& device)
            {
                add_channelmapping_controls(device.data.at(U("controls")), model.settings);
                nmos::set_resource_version(device, nmos::tai_now());
            });

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Added IS-08 Channel Mapping API to device controls";
        }

        void maybe_remove_device_channelmapping_control(nmos::node_model& model, slog::base_gate& gate)
        {
            if (!model.channelmapping_resources.empty()) return;

            const auto seed_id = nmos::experimental::fields::seed_id(model.settings);
            const auto device_id = impl::make_id(seed_id, nmos::types::device);

            const std::pair<nmos::id, nmos::type> id_type{ device_id, nmos::types::device };
            const auto device = nmos::find_resource(model.node_resources, id_type);
            if (model.node_resources.end() == device)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Could not find " << id_type;
                throw node_implementation_exception();
            }
            if (!has_channelmapping_control(device->data.at(U("controls")))) return;

            nmos::modify_resource(model.node_resources, device_id, [&](nmos::resource& device)
            {
                remove_channelmapping_controls(device.data.at(U("controls")));
                nmos::set_resource_version(device, nmos::tai_now());
            });

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Removed IS-08 Channel Mapping API from device controls";
        }
    }

    // This constructs and inserts IS-08 input/output resources into the model, based on the specified channel mapping configuration.
    void node_implementation_add_channelmapping_(nmos::node_model& model, const nvnmos::name& name, const channelmapping_config& mapping, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        if (name.empty()) throw std::invalid_argument("Channel mapping name must not be empty");
        if (mapping.inputs.empty() && mapping.outputs.empty()) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " must have at least one input or output");

        auto& channelmappings = nvnmos::fields::channelmappings(model.settings);
        if (channelmappings.has_field(name)) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " already exists");

        const auto seed_id = nmos::experimental::fields::seed_id(model.settings);

        std::vector<utility::string_t> input_ids;
        std::vector<utility::string_t> output_ids;
        input_ids.reserve(mapping.inputs.size());
        output_ids.reserve(mapping.outputs.size());

        for (const auto& input : mapping.inputs)
        {
            const auto& id = input.id;
            if (id.empty()) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " input id must not be empty");
            if (impl::channelmapping_ids_contains(channelmappings, nvnmos::fields::channelmapping_inputs, id)) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " input " + utility::us2s(id) + " already exists");
            if (input.channel_labels.empty()) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " input " + utility::us2s(id) + " must have at least one channel label");

            const auto parent_id = !input.parent_name.empty() ? impl::make_id(seed_id, input.parent_type, input.parent_name) : nmos::id{};
            const auto parent = std::make_pair(parent_id, parent_id.empty() ? nmos::type{} : input.parent_type);
            const auto reordering = 0 != input.block_size ? input.reordering : true;
            const auto block_size = 0 != input.block_size ? input.block_size : 1u;

            auto resource = nmos::make_channelmapping_input(id, input.name, input.description, parent, input.channel_labels, reordering, block_size);

            impl::insert_resource(model.channelmapping_resources, std::move(resource), gate);
            input_ids.push_back(id);
        }

        for (const auto& output : mapping.outputs)
        {
            const auto& id = output.id;
            if (id.empty()) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " output id must not be empty");
            if (impl::channelmapping_ids_contains(channelmappings, nvnmos::fields::channelmapping_outputs, id)) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " output " + utility::us2s(id) + " already exists");
            if (output.channel_labels.empty()) throw std::invalid_argument("Channel mapping " + utility::us2s(name) + " output " + utility::us2s(id) + " must have at least one channel label");

            const auto source_id = !output.sender_name.empty() ? impl::make_id(seed_id, nmos::types::source, output.sender_name) : nmos::id{};

            auto resource = nmos::make_channelmapping_output(id, output.name, output.description, source_id, output.channel_labels, output.routable_inputs);

            impl::insert_resource(model.channelmapping_resources, std::move(resource), gate);
            output_ids.push_back(id);
        }

        channelmappings[name] = value_of({
            { nvnmos::fields::channelmapping_inputs, web::json::value_from_elements(input_ids) },
            { nvnmos::fields::channelmapping_outputs, web::json::value_from_elements(output_ids) }
        });

        impl::maybe_add_device_channelmapping_control(model, gate);

        slog::log<slog::severities::info>(gate, SLOG_FLF)
            << "Added channel mapping " << name << " with " << input_ids.size() << " inputs and " << output_ids.size() << " outputs";
    }

    // This removes IS-08 input/output resources from the model corresponding to the specified name.
    void node_implementation_remove_channelmapping_(nmos::node_model& model, const nvnmos::name& name, slog::base_gate& gate)
    {
        using web::json::value;

        auto& channelmappings = nvnmos::fields::channelmappings(model.settings);
        if (!channelmappings.has_field(name)) throw std::invalid_argument("Could not find channel mapping with name: " + utility::us2s(name));

        const auto& config = channelmappings.at(name);

        const auto erase_ids = [&](const web::json::value& ids, const nmos::type& type)
        {
            for (const auto& id : ids.as_array())
            {
                const auto resource_id = nmos::make_channelmapping_resource_id({ id.as_string(), type });
                nmos::erase_resource(model.channelmapping_resources, resource_id);
            }
        };

        erase_ids(nvnmos::fields::channelmapping_inputs(config), nmos::types::input);
        erase_ids(nvnmos::fields::channelmapping_outputs(config), nmos::types::output);
        channelmappings.erase(name);

        impl::maybe_remove_device_channelmapping_control(model, gate);

        slog::log<slog::severities::info>(gate, SLOG_FLF) << "Removed channel mapping " << name;
    }

    // This updates the published active map for the specified output in the channel mapping, based on the specified active map.
    // `output_id` is the IS-08 output id. `active_map` length must equal that output's channel count; index i is output channel i.
    // Does not invoke the application's channel mapping activation callback.
    void node_implementation_activate_channelmapping_(nmos::node_model& model, const nvnmos::name& name, const nmos::channelmapping_id& output_id, const channelmapping_active_map& active_map, slog::base_gate& gate)
    {
        const auto& channelmappings = nvnmos::fields::channelmappings(model.settings);
        if (!channelmappings.has_field(name)) throw std::invalid_argument("Could not find channel mapping with name: " + utility::us2s(name));

        const auto& config = channelmappings.at(name);
        const auto& output_ids = nvnmos::fields::channelmapping_outputs(config);
        if (!impl::channelmapping_ids_contains(output_ids, output_id)) throw std::invalid_argument("Output " + utility::us2s(output_id) + " is not in channel mapping " + utility::us2s(name));

        const auto resource_id = nmos::make_channelmapping_resource_id({ output_id, nmos::types::output });
        const auto output = nmos::find_resource(model.channelmapping_resources, { resource_id, nmos::types::output });
        if (model.channelmapping_resources.end() == output)
        {
            // the channel mapping config references this output, so its resource must exist
            slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Could not find channel mapping output resource: " << output_id;
            throw node_implementation_exception();
        }

        const auto channel_count = nmos::fields::channels(nmos::fields::endpoint_io(output->data)).size();
        if (active_map.size() != channel_count) throw std::invalid_argument("Active map length " + std::to_string(active_map.size()) + " does not match output " + utility::us2s(output_id) + " channel count " + std::to_string(channel_count));

        const auto map_value = nmos::make_channelmapping_active_map(active_map);
        const auto activation_time = nmos::tai_now();

        nmos::modify_resource(model.channelmapping_resources, resource_id, [&](nmos::resource& resource)
        {
            nmos::fields::endpoint_active(resource.data)[nmos::fields::map] = map_value;
        });

        // Update the IS-04 source's version even though this is an out-of-band update (IS-08 §3.1)

        const auto& source_id_or_null = nmos::fields::endpoint_io(output->data).at(nmos::fields::source_id);
        if (!source_id_or_null.is_null())
        {
            nmos::modify_resource(model.node_resources, source_id_or_null.as_string(), [&](nmos::resource& source)
            {
                nmos::set_resource_version(source, activation_time);
            });
        }

        // Update the IS-04 device's version

        // hmm, we bump device version for every output activation, rather than once per batch...

        const auto seed_id = nmos::experimental::fields::seed_id(model.settings);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        nmos::modify_resource(model.node_resources, device_id, [&](nmos::resource& device)
        {
            nmos::set_resource_version(device, activation_time);
        });

        slog::log<slog::severities::info>(gate, SLOG_FLF)
            << "Published channelmapping active map for channel mapping " << name << " output " << output_id;
    }

    // This constructs and inserts IS-08 input/output resources into the model, based on the specified channel mapping configuration.
    void node_implementation_add_channelmapping(nmos::node_model& model, const nvnmos::name& name, const channelmapping_config& mapping, slog::base_gate& gate)
    {
        auto lock = model.write_lock();
        node_implementation_add_channelmapping_(model, name, mapping, gate);
        model.notify();
    }

    // This removes IS-08 input/output resources from the model corresponding to the specified name.
    void node_implementation_remove_channelmapping(nmos::node_model& model, const nvnmos::name& name, slog::base_gate& gate)
    {
        auto lock = model.write_lock();
        node_implementation_remove_channelmapping_(model, name, gate);
        model.notify();
    }

    // This updates the published active map for the specified output in the channel mapping, based on the specified active map.
    // `output_id` is the IS-08 output id. `active_map` length must equal that output's channel count; index i is output channel i.
    // Does not invoke the application's channel mapping activation callback.
    void node_implementation_activate_channelmapping(nmos::node_model& model, const nvnmos::name& name, const nmos::channelmapping_id& output_id, const channelmapping_active_map& active_map, slog::base_gate& gate)
    {
        auto lock = model.write_lock();
        node_implementation_activate_channelmapping_(model, name, output_id, active_map, gate);
        model.notify();
    }

    nmos::channelmapping_activation_handler make_node_implementation_channelmapping_activation_handler(channelmapping_activation_handler channelmapping_activated, nmos::settings& settings, slog::base_gate& gate)
    {
        return [&settings, channelmapping_activated, &gate](const nmos::resource& channelmapping_output)
        {
            const auto output_id = nmos::fields::channelmapping_id(channelmapping_output.data);
            const auto name = impl::name_for_output(nvnmos::fields::channelmappings(settings), output_id);
            if (name.empty())
            {
                slog::log<slog::severities::warning>(gate, SLOG_FLF) << "Channel mapping activation for unknown output: " << output_id;
                return;
            }

            const auto parsed = impl::parse_active_map_from_output(channelmapping_output);
            const bool success = channelmapping_activated
                ? channelmapping_activated(name, output_id, parsed)
                : true;
            if (!success)
            {
                slog::log<slog::severities::warning>(gate, SLOG_FLF)
                    << "Channel mapping activation failed for " << name << " output " << output_id;
            }
        };
    }

    // This constructs all the callbacks used to integrate the application into the server instance for the NMOS Node.
    nmos::experimental::node_implementation make_node_implementation(nmos::node_model& model, connection_activation_handler connection_activated, channelmapping_activation_handler channelmapping_activated, slog::base_gate& gate)
    {
        return nmos::experimental::node_implementation()
            .on_load_server_certificates(nmos::make_load_server_certificates_handler(model.settings, gate))
            .on_load_dh_param(nmos::make_load_dh_param_handler(model.settings, gate))
            .on_load_ca_certificates(nmos::make_load_ca_certificates_handler(model.settings, gate))
            .on_system_changed(make_node_implementation_system_global_handler(model, gate)) // may be omitted if not required
            .on_registration_changed(make_node_implementation_registration_handler(gate)) // may be omitted if not required
            .on_parse_transport_file(make_node_implementation_transport_file_parser()) // may be omitted if the default is sufficient
            .on_validate_connection_resource_patch(make_node_implementation_patch_validator()) // may be omitted if not required
            .on_resolve_auto(make_node_implementation_auto_resolver(model.settings))
            .on_set_transportfile(make_node_implementation_transportfile_setter(model.node_resources, model.settings))
            .on_connection_activated(make_node_implementation_connection_activation_handler(std::move(connection_activated), model.settings, gate))
            .on_channelmapping_activated(make_node_implementation_channelmapping_activation_handler(std::move(channelmapping_activated), model.settings, gate));
    }
}
