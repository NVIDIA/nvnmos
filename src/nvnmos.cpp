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

#include "nvnmos.h"

#include <boost/algorithm/string/join.hpp>
#include <boost/range/adaptor/transformed.hpp>
#include <boost/range/iterator_range_core.hpp>
#include "cpprest/host_utils.h"
#include "nmos/asset.h"
#include "nmos/log_gate.h"
#include "nmos/model.h"
#include "nmos/node_server.h"
#include "nmos/process_utils.h"
#include "nmos/server.h"
#include "nvnmos_impl.h"

namespace utility
{
    inline string_t s2us(const char* s)
    {
        if (0 == s) return {};
        return conversions::to_string_t(s);
    }
}

namespace nvnmos
{
    class log_gate : public slog::base_gate
    {
    public:
        log_gate(NvNmosNodeServer* server, nmos_logging_callback callback, nmos::experimental::log_model& model)
            : server(server)
            , callback(callback)
            , model(model)
        {}

        virtual bool pertinent(slog::severity level) const
        {
            return callback && model.level <= level;
        }

        virtual void log(const slog::log_message& message) const
        {
            if (callback)
            {
                auto categories = nmos::get_categories_stash(message.stream());
                auto csv = boost::join(categories, ",");
                callback(server, csv.c_str(), message.level(), message.str().c_str());
            }
        }

    private:
        NvNmosNodeServer* server;
        nmos_logging_callback callback;
        nmos::experimental::log_model& model;
    };

    class server
    {
    public:
        server(const NvNmosNodeConfig& config, NvNmosNodeServer* server);
        ~server();

        void add_receiver(const NvNmosReceiverConfig& config);
        void remove_receiver(const std::string& id);
        void add_sender(const NvNmosSenderConfig& config);
        void remove_sender(const std::string& id);

        void activate_rtp_connection(const std::string& id, const std::string& sdp);

    private:
        static nmos::settings make_settings(const NvNmosNodeConfig& config);
        void log_current_exception();

        nmos::node_model node_model;
        nmos::experimental::log_model log_model;
        log_gate gate;

        nmos::experimental::node_implementation node_implementation;
        std::unique_ptr<nmos::server> node_server;
    };

    server::server(const NvNmosNodeConfig& config, NvNmosNodeServer* server)
        : gate(server, config.log_callback, log_model)
    {
        using web::json::value_of;

        try
        {
            // Prepare settings

            node_model.settings = make_settings(config);

            log_model.settings = node_model.settings;
            log_model.level = nmos::fields::logging_level(log_model.settings);

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Starting NvNmos node";

            // Log the process ID and initial settings

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Process ID: " << nmos::details::get_process_id();
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Build settings: " << nmos::get_build_settings_info();
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Initial settings: " << node_model.settings.serialize();

            // Set up the callbacks between the node server and the underlying implementation

            const auto& activated = config.rtp_connection_activated;
            auto& gate_ = gate;
            auto rtp_connection_activated = [activated, server, &gate_](const std::string& id, const std::string& sdp)
            {
                if (!activated) return;
                const bool success = activated(server, id.c_str(), !sdp.empty() ? sdp.c_str() : 0);
                if (!success)
                {
                    slog::log<slog::severities::warning>(gate_, SLOG_FLF) << "Activation failed for internal id: " << id;
                }
            };
            node_implementation = make_node_implementation(node_model, rtp_connection_activated, gate);

            // Set up the node server

            node_server.reset(new nmos::server(nmos::experimental::make_node_server(node_model, node_implementation, log_model, gate)));

            // Disable TRACE method

            for (auto& http_listener : node_server->http_listeners)
            {
                http_listener.support(web::http::methods::TRCE, [](web::http::http_request req) { req.reply(web::http::status_codes::MethodNotAllowed); });
            }

            // Set up the node resources, etc.

            node_implementation_init(node_model, gate);

            for (auto& receiver : boost::make_iterator_range_n(config.receivers, config.num_receivers))
            {
                if (!receiver.sdp) throw std::logic_error("invalid receiver config");
                node_implementation_add_receiver(node_model, receiver.sdp, gate);
            }

            for (auto& sender : boost::make_iterator_range_n(config.senders, config.num_senders))
            {
                if (!sender.sdp) throw std::logic_error("invalid sender config");
                node_implementation_add_sender(node_model, sender.sdp, gate);
            }

            // Open the API ports and start up node operation (including the DNS-SD advertisements)

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Preparing for connections";

            node_server->open().wait();

            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Ready for connections";
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }

    server::~server()
    {
        if (!node_server) return;
        try
        {
            slog::log<slog::severities::info>(gate, SLOG_FLF) << "Closing connections";

            node_server->close().wait();
        }
        catch (...)
        {
            log_current_exception();
        }

        slog::log<slog::severities::info>(gate, SLOG_FLF) << "Stopping NvNmos node";
    }

    static const nmos::id seed_namespace_id = U("18daddcf-a234-4f59-808a-dbf6a42e17bb");

    nmos::settings server::make_settings(const NvNmosNodeConfig& config)
    {
        using web::json::value_from_elements;
        using web::json::value_of;

        nmos::settings settings;

        const auto host_name = 0 != config.host_name ? utility::s2us(config.host_name) : nmos::get_host_name({});
        const auto dot = host_name.find(U('.'));
        const auto domain = utility::string_t::npos != dot ? host_name.substr(dot + 1) : nmos::get_domain({});
        web::json::insert(settings, std::make_pair(nmos::fields::host_name, host_name));
        web::json::insert(settings, std::make_pair(nmos::fields::domain, domain));

        if (0 != config.label)
        {
            web::json::insert(settings, std::make_pair(nvnmos::fields::node_label, utility::s2us(config.label)));
            web::json::insert(settings, std::make_pair(nvnmos::fields::device_label, utility::s2us(config.label)));
        }
        else if (0 != config.asset_tags)
        {
            const auto& asset = *config.asset_tags;
            const auto asset_label = boost::algorithm::join(
                std::initializer_list<const char*>{ asset.manufacturer, asset.product, asset.instance_id } | boost::adaptors::transformed([](const char* s)
                {
                    return utility::s2us(s);
                }), L" "
            );
            web::json::insert(settings, std::make_pair(nvnmos::fields::node_label, asset_label));
            web::json::insert(settings, std::make_pair(nvnmos::fields::device_label, asset_label));
        }

        if (0 != config.description)
        {
            web::json::insert(settings, std::make_pair(nvnmos::fields::node_description, utility::s2us(config.description)));
            web::json::insert(settings, std::make_pair(nvnmos::fields::device_description, utility::s2us(config.description)));
        }
        else if (0 != config.asset_tags)
        {
            const auto& asset = *config.asset_tags;
            const auto asset_description = boost::algorithm::join(boost::make_iterator_range_n(asset.functions, asset.num_functions)
                | boost::adaptors::transformed([&](const char* function)
            {
                return utility::s2us(function);
            }), L", ");
            web::json::insert(settings, std::make_pair(nvnmos::fields::node_description, asset_description));
            web::json::insert(settings, std::make_pair(nvnmos::fields::device_description, asset_description));
        }

        if (0 != config.host_addresses && 0 != config.num_host_addresses)
        {
            const auto host_addresses = boost::copy_range<std::vector<utility::string_t>>(boost::make_iterator_range_n(config.host_addresses, config.num_host_addresses)
                | boost::adaptors::transformed([&](const char* host_address)
            {
                return utility::s2us(host_address);
            }));
            web::json::insert(settings, std::make_pair(nmos::fields::host_addresses, value_from_elements(host_addresses)));
        }

        web::json::insert(settings, std::make_pair(nmos::experimental::fields::href_mode, 3));

        if (0 != config.http_port)
        {
            web::json::insert(settings, std::make_pair(nmos::fields::http_port, config.http_port));
        }
        web::json::insert(settings, std::make_pair(nmos::fields::events_port, -1));
        web::json::insert(settings, std::make_pair(nmos::fields::events_ws_port, -1));
        web::json::insert(settings, std::make_pair(nmos::fields::channelmapping_port, -1));

        if (0 != config.asset_tags)
        {
            const auto& asset = *config.asset_tags;
            const auto manufacturer = utility::s2us(asset.manufacturer);
            const auto product = utility::s2us(asset.product);
            const auto instance_id = utility::s2us(asset.instance_id);
            const auto functions = boost::copy_range<std::vector<utility::string_t>>(boost::make_iterator_range_n(asset.functions, asset.num_functions)
                | boost::adaptors::transformed([&](const char* function)
            {
                return utility::s2us(function);
            }));
            web::json::insert(settings, std::make_pair(nvnmos::fields::node_tags, value_of({
                { nmos::fields::asset_manufacturer, value_of({ manufacturer }) },
                { nmos::fields::asset_product_name, value_of({ product }) },
                { nmos::fields::asset_instance_id, value_of({ instance_id }) }
            })));
            web::json::insert(settings, std::make_pair(nvnmos::fields::device_tags, value_of({
                { nmos::fields::asset_manufacturer, value_of({ manufacturer }) },
                { nmos::fields::asset_product_name, value_of({ product }) },
                { nmos::fields::asset_instance_id, value_of({ instance_id }) },
                { nmos::fields::asset_function, value_from_elements(functions) }
            })));
        }

        if (0 != config.seed)
        {
            auto seed_id = nmos::make_repeatable_id(seed_namespace_id, utility::s2us(config.seed));
            web::json::insert(settings, std::make_pair(nmos::experimental::fields::seed_id, std::move(seed_id)));
        }

        {
            web::json::insert(settings, std::make_pair(nmos::fields::logging_level, config.log_level));
        }

        if (0 != config.num_log_categories)
        {
            auto categories = value_from_elements(boost::make_iterator_range_n(config.log_categories, config.num_log_categories) | boost::adaptors::transformed([](const char* category)
            {
                if (!category) throw std::logic_error("invalid log category");
                return utility::s2us(category);
            }));
            web::json::insert(settings, std::make_pair(nmos::fields::logging_categories, std::move(categories)));
        }

        nmos::insert_node_default_settings(settings);

        return settings;
    }

    void server::log_current_exception()
    {
        try
        {
            throw;
        }
        catch (const node_implementation_exception&)
        {
            // node implementation writes the log message
        }
        catch (const web::json::json_exception& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "JSON error: " << e.what();
        }
        catch (const web::http::http_exception& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "HTTP error: " << e.what() << " [" << e.error_code() << "]";
        }
        catch (const web::websockets::websocket_exception& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "WebSocket error: " << e.what() << " [" << e.error_code() << "]";
        }
        catch (const std::ios_base::failure& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "File error: " << e.what();
        }
        catch (const std::system_error& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "System error: " << e.what() << " [" << e.code() << "]";
        }
        catch (const std::runtime_error& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "Implementation error: " << e.what();
        }
        catch (const std::exception& e)
        {
            slog::log<slog::severities::error>(gate, SLOG_FLF) << "Unexpected exception: " << e.what();
        }
        catch (...)
        {
            slog::log<slog::severities::severe>(gate, SLOG_FLF) << "Unexpected unknown exception";
        }
    }

    void server::add_receiver(const NvNmosReceiverConfig& config)
    {
        using web::json::value_of;

        try
        {
            if (!config.sdp) throw std::logic_error("invalid receiver config");
            node_implementation_add_receiver(node_model, config.sdp, gate);
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }

    void server::remove_receiver(const std::string& id)
    {
        try
        {
            node_implementation_remove_receiver(node_model, utility::s2us(id), gate);
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }

    void server::add_sender(const NvNmosSenderConfig& config)
    {
        using web::json::value_of;

        try
        {
            if (!config.sdp) throw std::logic_error("invalid sender config");
            node_implementation_add_sender(node_model, config.sdp, gate);
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }

    void server::remove_sender(const std::string& id)
    {
        try
        {
            node_implementation_remove_sender(node_model, utility::s2us(id), gate);
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }

    void server::activate_rtp_connection(const std::string& id, const std::string& sdp)
    {
        try
        {
            node_implementation_activate_rtp_connection(node_model, utility::s2us(id), sdp, gate);
        }
        catch (...)
        {
            log_current_exception();
            throw;
        }
    }
}

NVNMOS_API
bool create_nmos_node_server(
    const NvNmosNodeConfig* config,
    NvNmosNodeServer* server)
{
    if (!config || !server) return false;
    try
    {
        std::unique_ptr<nvnmos::server> impl(new nvnmos::server(*config, server));

        server->impl = impl.release();
        return true;
    }
    catch (...)
    {
        return false;
    }
}

NVNMOS_API
bool destroy_nmos_node_server(
    NvNmosNodeServer* server)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    delete impl;
    server->impl = 0;

    return true;
}

NVNMOS_API
bool add_nmos_receiver_to_node_server(
    NvNmosNodeServer* server,
    const NvNmosReceiverConfig* config)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    if (!impl) return false;
    if (!config) return false;

    try
    {
        impl->add_receiver(*config);
        return true;
    }
    catch (...)
    {
        return false;
    }
}

NVNMOS_API
bool remove_nmos_receiver_from_node_server(
    NvNmosNodeServer* server,
    const char* id)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    if (!impl) return false;
    if (!id) return false;

    try
    {
        impl->remove_receiver(id);
        return true;
    }
    catch (...)
    {
        return false;
    }
}

NVNMOS_API
bool add_nmos_sender_to_node_server(
    NvNmosNodeServer* server,
    const NvNmosSenderConfig* config)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    if (!impl) return false;
    if (!config) return false;

    try
    {
        impl->add_sender(*config);
        return true;
    }
    catch (...)
    {
        return false;
    }
}

NVNMOS_API
bool remove_nmos_sender_from_node_server(
    NvNmosNodeServer* server,
    const char* id)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    if (!impl) return false;
    if (!id) return false;

    try
    {
        impl->remove_sender(id);
        return true;
    }
    catch (...)
    {
        return false;
    }
}

NVNMOS_API
bool nmos_connection_rtp_activate(
    NvNmosNodeServer* server,
    const char* id,
    const char* sdp)
{
    if (!server) return false;
    auto impl = (nvnmos::server*)server->impl;
    if (!impl) return false;
    if (!id) return false;
    if (!sdp) sdp = "";

    try
    {
        impl->activate_rtp_connection(id, sdp);
        return true;
    }
    catch (...)
    {
        return false;
    }
}
