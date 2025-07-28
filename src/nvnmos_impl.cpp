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
#include <boost/algorithm/string/predicate.hpp>
#include <boost/asio/ip/address_v4.hpp>
#include <boost/range/adaptor/filtered.hpp>
#include <boost/range/adaptor/transformed.hpp>
#include <boost/range/algorithm/find.hpp>
#include <boost/range/algorithm/find_if.hpp>
#include <boost/range/irange.hpp>
#include "cpprest/host_utils.h"
#include "nmos/activation_mode.h"
#include "nmos/activation_utils.h"
#include "nmos/capabilities.h"
#include "nmos/channels.h"
#include "nmos/clock_name.h"
#include "nmos/clock_ref_type.h"
#include "nmos/colorspace.h"
#include "nmos/connection_resources.h"
#include "nmos/format.h"
#include "nmos/group_hint.h"
#include "nmos/interlace_mode.h"
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
#include "nmos/transport.h"
#include "nmos/video_jxsv.h"
#include "sdp/sdp.h"

namespace nvnmos
{
    namespace fields
    {
        const web::json::field_as_value_or internal_id_tag{ U("urn:x-nvnmos:id"), web::json::value::array() };
    }

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
        web::json::value make_session_description(const nmos::type& type, const utility::string_t& internal_id, const utility::string_t& group_hint, const utility::string_t& session_info, const nmos::sdp_parameters& sdp_params, const web::json::value& transport_params);

        // like nmos::get_session_description_sdp_parameters
        // with support for multiple ts-refclk attributes in each media description
        std::vector<std::vector<nmos::sdp_parameters::ts_refclk_t>> get_session_description_ts_refclks(const web::json::value& session_description);

        // like nmos::get_session_description_transport_params
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value get_session_description_transport_params(const nmos::type& type, const web::json::value& session_description);

        // get the internal id from the custom attribute
        utility::string_t get_session_description_internal_id(const web::json::value& session_description);

        // get the optional group hint from the custom attribute
        utility::string_t get_session_description_group_hint(const web::json::value& session_description);

        // get the optional session information
        utility::string_t get_session_description_session_info(const web::json::value& session_description);

        // get the format bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_format_bit_rate(const nmos::sdp_parameters& sdp_params);
        // get the transport bit rate from the custom attribute if present or calculate an approximate value
        uint64_t get_transport_bit_rate(const nmos::sdp_parameters& sdp_params);

        // find interface with the specified address
        std::vector<web::hosts::experimental::host_interface>::const_iterator find_interface(const std::vector<web::hosts::experimental::host_interface>& interfaces, const utility::string_t& address);

        // generate repeatable ids for the node's resources
        nmos::id make_id(const nmos::id& seed_id, const nmos::type& type, const utility::string_t& internal_id = {});

        // generate a repeatable source-specific multicast address for each leg of a sender
        utility::string_t make_source_specific_multicast_address_v4(const nmos::id& id, int leg);

        // set the internal id for the sender or receiver as a resource tag
        void set_internal_id(nmos::resource& resource, const utility::string_t& internal_id);
        // get the internal id for the sender or receiver from a resource tag
        utility::string_t get_internal_id(const nmos::resource& resource);

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

        // modify node resource if necessary to include all of the specified interfaces that currently have interface_bindings in any senders or receivers
        void update_node_interfaces(nmos::resources& node_resources, const nmos::id& node_id, const std::vector<web::hosts::experimental::host_interface>& host_interfaces);
    }

    // forward declarations
    nmos::connection_resource_auto_resolver make_node_implementation_auto_resolver();

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
            if (!nmos::insert_resource(node_resources, std::move(node)).second) throw node_implementation_exception();
        }

        // device
        {
            auto device = nmos::make_device(device_id, node_id, {}, {}, settings);
            device.data[nmos::fields::label] = value::string(nvnmos::fields::device_label(settings));
            device.data[nmos::fields::description] = value::string(nvnmos::fields::device_description(settings));
            device.data[nmos::fields::tags] = nvnmos::fields::device_tags(settings);
            if (!nmos::insert_resource(node_resources, std::move(device)).second) throw node_implementation_exception();
        }

        // insert empty clock, sender and receiver configs
        settings[nvnmos::fields::clocks] = value::object();
        settings[nvnmos::fields::senders] = value::object();
        settings[nvnmos::fields::receivers] = value::object();
    }

    void node_implementation_add_sender_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& sdp_, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto sdp = sdp::parse_session_description(sdp_);
        const auto sdp_params = nmos::get_session_description_sdp_parameters(sdp);
        const auto ts_refclks = impl::get_session_description_ts_refclks(sdp);
        const auto transport_params = impl::get_session_description_transport_params(nmos::types::sender, sdp);
        const auto internal_id = impl::get_session_description_internal_id(sdp);
        // hm, could check the internal id is unique across all senders and receivers
        const auto group_hint = impl::get_session_description_group_hint(sdp);
        const auto session_info = impl::get_session_description_session_info(sdp);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto source_id = impl::make_id(seed_id, nmos::types::source, internal_id);
        const auto flow_id = impl::make_id(seed_id, nmos::types::flow, internal_id);
        const auto sender_id = impl::make_id(seed_id, nmos::types::sender, internal_id);

        // for now, only manage a single clock
        const auto clock = nmos::clock_names::clk0;

        const auto media_type = nmos::get_media_type(sdp_params);
        const auto format = impl::get_format(media_type);

        const auto interface_names = boost::copy_range<std::vector<utility::string_t>>(
            transport_params.as_array() | boost::adaptors::transformed([&](const value& transport_param)
        {
            const auto& address = nmos::fields::source_ip(transport_param).as_string();
            const auto interface = impl::find_interface(host_interfaces, address);
            if (host_interfaces.end() == interface)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF)
                    << "No network interface corresponding to the connection address: " << address << " for: " << internal_id;
                throw node_implementation_exception();
            }
            return interface->name;
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
            constraints[leg][nmos::fields::source_ip] = value_of({
                { nmos::fields::constraint_enum, value_of({ nmos::fields::source_ip(transport_params.at(leg)) }) }
            });
        }

        const auto resolve_auto = make_node_implementation_auto_resolver();
        resolve_auto(sender, connection_sender, connection_sender.data[nmos::fields::endpoint_active][nmos::fields::transport_params]);

        // override default label and description from model.settings
        sender.data[nmos::fields::label] = value::string(sdp_params.session_name);
        sender.data[nmos::fields::description] = value::string(session_info);
        // set the internal id as a resource tag
        impl::set_internal_id(sender, internal_id);
        // set the group hint as a resource tag
        if (!group_hint.empty()) impl::set_group_hint(sender, group_hint);

        if (!insert_resource(node_resources, std::move(source)).second) throw node_implementation_exception();
        if (!insert_resource(node_resources, std::move(flow)).second) throw node_implementation_exception();
        if (!insert_resource(node_resources, std::move(sender)).second) throw node_implementation_exception();
        if (!insert_resource(connection_resources, std::move(connection_sender)).second) throw node_implementation_exception();

        // update device's deprecated senders array

        nmos::modify_resource(node_resources, device_id, [&](nmos::resource& device)
        {
            device.data[nmos::fields::version] = value::string(nmos::make_version());
            web::json::push_back(nmos::fields::senders(device.data), sender_id);
        });

        // update node's interfaces

        impl::update_node_interfaces(node_resources, node_id, host_interfaces);

        // update node's clocks

        auto& clock_settings = nvnmos::fields::clocks(settings)[clock.name];
        auto ptp_domain = nmos::fields::ptp_domain_number(clock_settings);
        impl::update_node_clock(node_resources, node_id, impl::make_node_clock(clock, ts_refclks, ptp_domain));

        clock_settings[nmos::fields::ptp_domain_number] = ptp_domain;

        // insert into settings

        nvnmos::fields::senders(settings)[sender_id] = value_of({
            { nvnmos::fields::sdp, utility::s2us(sdp_) }
        });
    }

    void node_implementation_add_receiver_(nmos::resources& node_resources, nmos::resources& connection_resources, const std::string& sdp_, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto sdp = sdp::parse_session_description(sdp_);
        const auto sdp_params = nmos::get_session_description_sdp_parameters(sdp);
        const auto transport_params = impl::get_session_description_transport_params(nmos::types::receiver, sdp);
        const auto internal_id = impl::get_session_description_internal_id(sdp);
        // hm, could check the internal id is unique across all senders and receivers
        const auto group_hint = impl::get_session_description_group_hint(sdp);
        const auto session_info = impl::get_session_description_session_info(sdp);

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto device_id = impl::make_id(seed_id, nmos::types::device);
        const auto receiver_id = impl::make_id(seed_id, nmos::types::receiver, internal_id);

        const auto media_type = nmos::get_media_type(sdp_params);
        const auto format = impl::get_format(media_type);

        const auto interface_names = boost::copy_range<std::vector<utility::string_t>>(
            transport_params.as_array() | boost::adaptors::transformed([&](const value& transport_param)
        {
            const auto& address = nmos::fields::interface_ip(transport_param).as_string();
            const auto interface = impl::find_interface(host_interfaces, address);
            if (host_interfaces.end() == interface)
            {
                slog::log<slog::severities::severe>(gate, SLOG_FLF)
                    << "No network interface corresponding to the connection address: " << address << " for: " << internal_id;
                throw node_implementation_exception();
            }
            return interface->name;
        }));

        nmos::resource receiver;

        if (impl::format::video == format)
        {
            receiver = nmos::make_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, nmos::formats::video, { media_type }, settings);

            // add a constraint set; these should be completed fully!
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
        else if (impl::format::audio == format)
        {
            const auto audio = nmos::get_audio_L_parameters(sdp_params);

            receiver = nmos::make_audio_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, audio.bit_depth, settings);
            // add a constraint set; these should be completed fully!
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
        else if (impl::format::data == format)
        {
            const auto data = nmos::get_video_smpte291_parameters(sdp_params);

            receiver = nmos::make_sdianc_data_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, settings);
            // add a constraint set; these should be completed fully!
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
        else if (impl::format::mux == format)
        {
            const auto mux = nmos::get_video_SMPTE2022_6_parameters(sdp_params);

            receiver = nmos::make_mux_receiver(receiver_id, device_id, nmos::transports::rtp, interface_names, settings);
            // hmm, add a constraint set, e.g. taking account of sdp_params.framerate
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

        const auto resolve_auto = make_node_implementation_auto_resolver();
        resolve_auto(receiver, connection_receiver, connection_receiver.data[nmos::fields::endpoint_active][nmos::fields::transport_params]);

        // override default label and description from settings
        receiver.data[nmos::fields::label] = value::string(sdp_params.session_name);
        receiver.data[nmos::fields::description] = value::string(session_info);
        // set the internal id as a resource tag
        impl::set_internal_id(receiver, internal_id);
        // set the group hint as a resource tag
        if (!group_hint.empty()) impl::set_group_hint(receiver, group_hint);

        if (!insert_resource(node_resources, std::move(receiver)).second) throw node_implementation_exception();
        if (!insert_resource(connection_resources, std::move(connection_receiver)).second) throw node_implementation_exception();

        // update device's deprecated receivers array

        nmos::modify_resource(node_resources, device_id, [&](nmos::resource& device)
        {
            device.data[nmos::fields::version] = value::string(nmos::make_version());
            web::json::push_back(nmos::fields::receivers(device.data), receiver_id);
        });

        // update node's interfaces

        impl::update_node_interfaces(node_resources, node_id, host_interfaces);

        // insert into settings

        nvnmos::fields::receivers(settings)[receiver_id] = value_of({
            { nvnmos::fields::sdp, utility::s2us(sdp_) }
        });
    }

    void node_implementation_remove_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const nmos::type& type, const utility::string_t& internal_id, const std::vector<web::hosts::experimental::host_interface>& host_interfaces, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        // find sender or receiver with specified internal id

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto id = impl::make_id(seed_id, type, internal_id);
        auto resource = nmos::find_resource(node_resources, { id, type });

        if (node_resources.end() != resource)
        {
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

            // update device's deprecated senders/receivers array

            nmos::modify_resource(node_resources, device_id, [&](nmos::resource& device)
            {
                auto& refs = nmos::types::sender == type ? nmos::fields::senders(device.data) : nmos::fields::receivers(device.data);
                auto ref = std::find(refs.begin(), refs.end(), value::string(id));
                if (refs.end() != ref)
                {
                    device.data[nmos::fields::version] = value::string(nmos::make_version());

                    refs.erase(ref);
                }
            });

            // update node's interfaces

            impl::update_node_interfaces(node_resources, node_id, host_interfaces);

            // erase from settings

            auto& configs = nmos::types::sender == type ? nvnmos::fields::senders(settings) : nvnmos::fields::receivers(settings);
            if (configs.has_field(id))
            {
                configs.erase(id);
            }
        }
        else
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "Could not find " << type.name << " with internal id: " << internal_id;
            throw node_implementation_exception();
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

    // This constructs and inserts sources/flows/senders into the model, based on the specified SDP file.
    void node_implementation_add_sender(nmos::node_model& model, const std::string& sdp, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_add_sender_(model.node_resources, model.connection_resources, sdp, host_interfaces, model.settings, gate);

        model.notify();
    }

    // This constructs and inserts a receiver into the model, based on the specified SDP file.
    void node_implementation_add_receiver(nmos::node_model& model, const std::string& sdp, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_add_receiver_(model.node_resources, model.connection_resources, sdp, host_interfaces, model.settings, gate);

        model.notify();
    }

    // This removes sources/flows/senders from the model corresponding to the specified id.
    void node_implementation_remove_sender(nmos::node_model& model, const utility::string_t& internal_id, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_remove_connection_(model.node_resources, model.connection_resources, nmos::types::sender, internal_id, host_interfaces, model.settings, gate);

        model.notify();
    }

    // This removes the receiver from the model corresponding to the specified id.
    void node_implementation_remove_receiver(nmos::node_model& model, const utility::string_t& internal_id, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        const auto host_interfaces = web::hosts::experimental::host_interfaces();

        node_implementation_remove_connection_(model.node_resources, model.connection_resources, nmos::types::receiver, internal_id, host_interfaces, model.settings, gate);

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

    // Connection API activation callback to resolve "auto" values when /staged is transitioned to /active
    nmos::connection_resource_auto_resolver make_node_implementation_auto_resolver()
    {
        using web::json::value;

        return [](const nmos::resource& resource, const nmos::resource& connection_resource, value& transport_params)
        {
            const std::pair<nmos::id, nmos::type> id_type{ connection_resource.id, connection_resource.type };
            // this code relies on the specific constraints added by node_implementation_init
            const auto& constraints = nmos::fields::endpoint_constraints(connection_resource.data);

            const auto is_rtp = nmos::transports::rtp == nmos::transport_base(nmos::transport{ nmos::fields::transport(resource.data) });

            // "In some cases the behaviour is more complex, and may be determined by the vendor."
            // See https://specs.amwa.tv/is-05/releases/v1.0.0/docs/2.2._APIs_-_Server_Side_Implementation.html#use-of-auto
            if (nmos::types::sender == id_type.second && is_rtp)
            {
                for (int leg = 0; leg < (int)constraints.size(); ++leg)
                {
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::source_ip, [&] { return web::json::front(nmos::fields::constraint_enum(constraints.at(leg).at(nmos::fields::source_ip))); });
                    nmos::details::resolve_auto(transport_params[leg], nmos::fields::destination_ip, [&] { return value::string(impl::make_source_specific_multicast_address_v4(id_type.first, leg)); });
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
                const auto& sdp_data = nvnmos::fields::sdp(config->second);

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
                sdp_params.origin.session_version = sdp::ntp_now() >> 32;

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
    nmos::connection_activation_handler make_node_implementation_connection_activation_handler(rtp_connection_activation_handler rtp_connection_activated, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_from_elements;

        return [&settings, rtp_connection_activated, &gate](const nmos::resource& resource, const nmos::resource& connection_resource)
        {
            const std::pair<nmos::id, nmos::type> id_type{ resource.id, resource.type };
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Activating " << id_type;

            auto& configs = nmos::types::sender == resource.type ? nvnmos::fields::senders(settings) : nvnmos::fields::receivers(settings);
            auto config = configs.as_object().find(resource.id);

            const auto is_rtp = nmos::transports::rtp == nmos::transport_base(nmos::transport{ nmos::fields::transport(resource.data) });

            if (configs.as_object().end() != config && is_rtp)
            {
                const auto internal_id = impl::get_internal_id(resource);

                const auto& endpoint_active = nmos::fields::endpoint_active(connection_resource.data);

                // determine the new state of the sender or receiver
                const bool active = nmos::fields::master_enable(endpoint_active);

                if (active)
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
                        : nvnmos::fields::sdp(config->second);

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
                    sdp_params.origin.session_version = sdp::ntp_now() >> 32;

                    const auto group_hint = impl::get_group_hint(resource);
                    const auto& session_info = nmos::fields::description(resource.data);
                    const auto merged_sdp = impl::make_session_description(id_type.second, internal_id, group_hint, session_info, sdp_params, transport_params);
                    const auto sdp_data = sdp::make_session_description(merged_sdp);

                    rtp_connection_activated(utility::us2s(internal_id), sdp_data);
                }
                else
                {
                    // deactivate sender or receiver

                    rtp_connection_activated(utility::us2s(internal_id), {});
                }
            }
        };
    }

    void node_implementation_activate_rtp_connection_(nmos::resources& node_resources, nmos::resources& connection_resources, const utility::string_t& internal_id, const std::string& sdp, nmos::settings& settings, slog::base_gate& gate)
    {
        using web::json::value;
        using web::json::value_of;

        const auto set_transportfile = make_node_implementation_transportfile_setter(node_resources, settings);

        // find sender or receiver with specified internal id

        const auto seed_id = nmos::experimental::fields::seed_id(settings);
        const auto node_id = impl::make_id(seed_id, nmos::types::node);
        const auto sender_id = impl::make_id(seed_id, nmos::types::sender, internal_id);
        const auto receiver_id = impl::make_id(seed_id, nmos::types::receiver, internal_id);

        auto resource = nmos::find_resource(node_resources, { sender_id, nmos::types::sender });
        if (node_resources.end() == resource)
        {
            resource = nmos::find_resource(node_resources, { receiver_id, nmos::types::receiver });
        }

        if (node_resources.end() != resource)
        {
            // hmm, consider how to handle this 'internal' activation
            // * for now, setting /active endpoint directly, cf. nmos::connection_activation_thread
            // * alternatively, by setting or patching /staged with an immediate or scheduled activation

            const std::pair<nmos::id, nmos::type> id_type{ resource->id, resource->type };
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Updating " << id_type << " with internal id: " << internal_id;

            if (nmos::types::sender == id_type.second && !sdp.empty())
            {
                auto source = impl::find_source_for_sender(node_resources, *resource);
                if (node_resources.end() == source) throw node_implementation_exception();
                auto& clock_or_null = nmos::fields::clock_name(source->data);
                if (clock_or_null.is_null()) throw node_implementation_exception();
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
                    set_transportfile(*resource, connection_resource, connection_resource.data[nmos::fields::endpoint_transportfile]);
                }
            });

            nmos::modify_resource(node_resources, id_type.first, [&](nmos::resource& resource)
            {
                nmos::set_resource_subscription(resource, !sdp.empty(), {}, activation_time);
            });
        }
        else
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "Could not find sender or receiver with internal id: " << internal_id;
        }
    }

    // This updates the transport parameters and transport file for the specified sender or receiver based on the specified SDP file.
    // For now, the SDP file is not validated against the existing sender or receiver capabilities and constraints.
    void node_implementation_activate_rtp_connection(nmos::node_model& model, const utility::string_t& internal_id, const std::string& sdp, slog::base_gate& gate)
    {
        auto lock = model.write_lock(); // in order to update the resources

        node_implementation_activate_rtp_connection_(model.node_resources, model.connection_resources, internal_id, sdp, model.settings, gate);

        model.notify();
    }

    namespace impl
    {
        // like nmos::make_session_description for 'internal' use
        // with support for the custom SDP attributes in nvnmos::attributes for senders as well as receivers
        web::json::value make_session_description(const nmos::type& type, const utility::string_t& internal_id, const utility::string_t& group_hint, const utility::string_t& session_info, const nmos::sdp_parameters& sdp_params, const web::json::value& transport_params)
        {
            using web::json::value;

            auto session_description = nmos::make_session_description(sdp_params, transport_params);

            {
                // using op[] rather than at because there can be no session-level attributes
                auto& session_attributes = session_description[sdp::fields::attributes];
                web::json::push_back(session_attributes, sdp::named_value(nvnmos::attributes::internal_id, internal_id));
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

                if (nmos::types::sender == type)
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
                    auto destination_ip = transport_param[!transport_param[nmos::fields::multicast_ip].is_null() ? nmos::fields::multicast_ip : nmos::fields::interface_ip];
                    transport_param[nmos::fields::destination_ip] = std::move(destination_ip);
                    transport_param.erase(nmos::fields::multicast_ip);
                    transport_param.erase(nmos::fields::interface_ip);
                    // hm, source port is unknown unless the custom SDP attribute is present...
                    // in the /active endpoint this could be indicated by unresolved "auto" or zero?
                    transport_param[nmos::fields::source_port] = value(U("auto"));
                }

                const auto& media_description = media_descriptions.at(leg);
                const auto& media_attributes = sdp::fields::attributes(media_description);
                {
                    const auto& ma = media_attributes.as_array();

                    auto interface_ip = sdp::find_name(ma, nvnmos::attributes::interface_ip);
                    if (ma.end() != interface_ip)
                    {
                        transport_param[nmos::types::sender == type ? nmos::fields::source_ip : nmos::fields::interface_ip] = sdp::fields::value(*interface_ip);
                    }

                    if (nmos::types::sender == type)
                    {
                        auto source_port = sdp::find_name(ma, nvnmos::attributes::source_port);
                        if (ma.end() != source_port)
                        {
                            auto sp = utility::istringstreamed(sdp::fields::value(*source_port).as_string(), 0);
                            transport_param[nmos::fields::source_port] = value(sp);
                        }
                    }

                    // set rtp_enabled to false in legs for media descriptions which include an 'a=inactive' attribute line
                    auto inactive = sdp::find_name(ma, sdp::attributes::inactive);
                    if (ma.end() != inactive)
                    {
                        transport_param[nmos::fields::rtp_enabled] = value::boolean(false);
                    }
                }
            }

            return transport_params;
        }

        // get the internal id from the custom attribute
        utility::string_t get_session_description_internal_id(const web::json::value& session_description)
        {
            const auto& session_attributes = sdp::fields::attributes(session_description);
            {
                const auto& sa = session_attributes.as_array();

                auto internal_id = sdp::find_name(sa, nvnmos::attributes::internal_id);
                if (sa.end() != internal_id)
                {
                    return sdp::fields::value(*internal_id).as_string();
                }
            }

            return U("");
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
            if (nmos::media_types::video_raw == media_type) return format::video;
            if (nmos::media_types::video_jxsv == media_type) return format::video;
            if (nmos::media_types::audio_L(24) == media_type) return format::audio;
            if (nmos::media_types::audio_L(16) == media_type) return format::audio;
            if (nmos::media_types::video_smpte291 == media_type) return format::data;
            if (nmos::media_types::video_SMPTE2022_6 == media_type) return format::mux;
            throw node_implementation_exception{};
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

        // generate repeatable ids for the node's resources
        nmos::id make_id(const nmos::id& seed_id, const nmos::type& type, const utility::string_t& internal_id)
        {
            return nmos::make_repeatable_id(seed_id, U("/x-nmos/node/") + type.name + U('/') + internal_id);
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

        // set the internal id for the sender or receiver as a resource tag
        void set_internal_id(nmos::resource& resource, const utility::string_t& internal_id)
        {
            using web::json::value_of;

            resource.data[nmos::fields::tags][nvnmos::fields::internal_id_tag] = value_of({ internal_id });
        }

        // get the internal id for the sender or receiver from a resource tag
        utility::string_t get_internal_id(const nmos::resource& resource)
        {
            const auto& tags = resource.data.at(nmos::fields::tags);
            const auto& internal_ids = nvnmos::fields::internal_id_tag(tags);
            return 0 != internal_ids.as_array().size()
                ? internal_ids.as_array().begin()->as_string()
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
            const auto& group_hints = nmos::fields::group_hint(tags);
            return 0 != group_hints.as_array().size()
                ? group_hints.as_array().begin()->as_string()
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

        // modify node resource if necessary to include all of the specified interfaces that currently have interface_bindings in any senders or receivers
        void update_node_interfaces(nmos::resources& node_resources, const nmos::id& node_id, const std::vector<web::hosts::experimental::host_interface>& host_interfaces)
        {
            using web::json::value;

            auto node = nmos::find_resource(node_resources, { node_id, nmos::types::node });
            if (node_resources.end() == node) throw node_implementation_exception();

            std::set<utility::string_t> interface_names;

            auto& by_type = node_resources.get<nmos::tags::type>();

            const auto senders = by_type.equal_range(nmos::details::has_data(nmos::types::sender));
            for (auto sender = senders.first; senders.second != sender; ++sender)
            {
                for (const auto& interface_binding : nmos::fields::interface_bindings(sender->data))
                {
                    interface_names.insert(interface_binding.as_string());
                }
            }

            const auto receivers = by_type.equal_range(nmos::details::has_data(nmos::types::receiver));
            for (auto receiver = receivers.first; receivers.second != receiver; ++receiver)
            {
                for (const auto& interface_binding : nmos::fields::interface_bindings(receiver->data))
                {
                    interface_names.insert(interface_binding.as_string());
                }
            }

            auto interfaces = nmos::make_node_interfaces(nmos::experimental::node_interfaces(boost::copy_range<std::vector<web::hosts::experimental::host_interface>>(host_interfaces
                | boost::adaptors::filtered([&](const web::hosts::experimental::host_interface& interface)
            {
                return interface_names.end() != interface_names.find(interface.name);
            }))));

            if (interfaces.as_array() != nmos::fields::interfaces(node->data))
            {
                nmos::modify_resource(node_resources, node_id, [&interfaces](nmos::resource& node)
                {
                    node.data[nmos::fields::version] = value::string(nmos::make_version());

                    node.data[nmos::fields::interfaces] = interfaces;
                });
            }
        }
    }

    // This constructs all the callbacks used to integrate the application into the server instance for the NMOS Node.
    nmos::experimental::node_implementation make_node_implementation(nmos::node_model& model, rtp_connection_activation_handler rtp_connection_activated, slog::base_gate& gate)
    {
        return nmos::experimental::node_implementation()
            .on_load_server_certificates(nmos::make_load_server_certificates_handler(model.settings, gate))
            .on_load_dh_param(nmos::make_load_dh_param_handler(model.settings, gate))
            .on_load_ca_certificates(nmos::make_load_ca_certificates_handler(model.settings, gate))
            .on_system_changed(make_node_implementation_system_global_handler(model, gate)) // may be omitted if not required
            .on_registration_changed(make_node_implementation_registration_handler(gate)) // may be omitted if not required
            .on_parse_transport_file(make_node_implementation_transport_file_parser()) // may be omitted if the default is sufficient
            .on_validate_connection_resource_patch(make_node_implementation_patch_validator()) // may be omitted if not required
            .on_resolve_auto(make_node_implementation_auto_resolver())
            .on_set_transportfile(make_node_implementation_transportfile_setter(model.node_resources, model.settings))
            .on_connection_activated(make_node_implementation_connection_activation_handler(std::move(rtp_connection_activated), model.settings, gate));
    }
}
