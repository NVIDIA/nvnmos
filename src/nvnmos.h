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

/**
 * @file nvnmos.h
 * <b>NVIDIA Networked Media Open Specifications (NMOS) API</b>
 *
 * @b Description: This file defines the NVIDIA NMOS utility library
 * (NvNmos) API.
 */

/**
 * @defgroup  nvnmos  Networked Media Open Specifications (NMOS) API
 *
 * Defines the NVIDIA NMOS utility library (NvNmos) API.
 *
 * The NvNmos utility library provides the APIs to create, destroy and
 * internally manage an <a href="https://specs.amwa.tv/nmos/">NMOS</a> Node for a Media Node application.
 *
 * The library can automatically discover and register with an NMOS Registry
 * on the network using the <a href="https://specs.amwa.tv/is-04/">AMWA IS-04</a> Registration API.
 *
 * The library provides callbacks for NMOS events such as <a href="https://specs.amwa.tv/is-05/">AMWA IS-05</a>
 * Connection API requests from an NMOS Controller. These callbacks can be
 * used to update running DeepStream pipelines with new transport parameters,
 * for example.
 *
 * NvNmos currently supports Senders and Receivers for uncompressed Video
 * and Audio, i.e., SMPTE ST 2110-20 and SMPTE ST 2110-30 streams.
 *
 * The NvNmos library supports the following specifications, using the <a href="https://github.com/sony/nmos-cpp">Sony nmos-cpp</a> implementation:
 * - <a href="https://specs.amwa.tv/is-04/">AMWA IS-04 NMOS Discovery and Registration Specification</a> v1.3
 * - <a href="https://specs.amwa.tv/is-05/">AMWA IS-05 NMOS Device Connection Management Specification</a> v1.1
 * - <a href="https://specs.amwa.tv/is-09/">AMWA IS-09 NMOS System Parameters Specification</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-002-01/">AMWA BCP-002-01 Natural Grouping of NMOS Resources</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-002-02/">AMWA BCP-002-02 NMOS Asset Distinguishing Information</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-004-01/">AMWA BCP-004-01 NMOS Receiver Capabilities</a> v1.0
 * - Session Description Protocol conforming to SMPTE ST 2110-20 and -30
 *
 * @ingroup NvNmosApi
 * @{
 */

#ifndef NVNMOS_H
#define NVNMOS_H

#if defined(NVNMOS_EXPORTS)

#if defined(_WIN32) || defined(__CYGWIN__)
#define NVNMOS_API __declspec(dllexport)
#elif defined(__GNUC__) && (__GNUC__ >= 4)
#define NVNMOS_API __attribute__ ((visibility("default")))
#else
#define NVNMOS_API
#endif

#elif defined(NVNMOS_STATIC)

#define NVNMOS_API

#else

#if defined(_WIN32) || defined(__CYGWIN__)
#define NVNMOS_API __declspec(dllimport)
#elif defined(__GNUC__) && (__GNUC__ >= 4)
#define NVNMOS_API
#else
#define NVNMOS_API
#endif

#endif

#include <stdbool.h>

#ifdef __cplusplus
extern "C"
{
#endif

typedef struct _NvNmosNodeServer NvNmosNodeServer;

/**
 * Type for a callback from NvNmos library when an IS-05 Connection API
 * activation occurs.
 *
 * @param[in] server A pointer to the server issuing the callback.
 * @param[in] id     The unique identifier for the sender or receiver
 *                   to be activated or deactivated.
 * @param[in] sdp    The updated Session Description Protocol data
 *                   for the sender or receiver, or a null pointer when
 *                   the sender or receiver is being deactivated.
 *                   The new data only updates the transport parameters
 *                   of the sender or receiver, not the media format.
 *                   The 'inactive' media-level attribute is used to
 *                   indicate a disabled leg.
 *                   The 'x-nvnmos-id' session-level attribute specifies
 *                   the unique identifier for the sender or receiver,
 *                   @p id.
 *                   For a receiver, the 'x-nvnmos-iface-ip' media-level
 *                   attribute is used to specify the interface IP
 *                   address on which the stream is received.
 *                   For a sender, the 'x-nvnmos-src-port' media-level
 *                   attribute is used to specify the source port
 *                   from which the stream is transmitted.
 * @return Whether the activation could be applied.
 */
typedef bool (* nmos_connection_rtp_activation_callback)(
    NvNmosNodeServer *server,
    const char *id,
    const char *sdp);

/**
 * Defines some common severity/logging levels for log messages from
 * the NvNmos library.
 */
enum {
    /** Low level debugging information. */
    NVNMOS_LOG_DEVEL = -40,
    /** Chatty messages such as detailed API request/response tracking. */
    NVNMOS_LOG_VERBOSE = -10,
    /** Higher level information about expected API events. */
    NVNMOS_LOG_INFO = 0,
    /** Minor problems that could be recovered automatically by the library. */
    NVNMOS_LOG_WARNING = 10,
    /** More serious recoverable errors such as rejected requests. */
    NVNMOS_LOG_ERROR = 20,
    /** Errors which are unlikely to be recoverable without restarting the server. */
    NVNMOS_LOG_SEVERE = 30,
    /** Errors which are likely to cause the server to immediately terminate. */
    NVNMOS_LOG_FATAL = 40
};

/**
 * Type for a callback from NvNmos library for log messages.
 *
 * @param[in] server     A pointer to the server issuing the callback.
 * @param[in] categories A comma separated list of topics, indicating
 *                       e.g. the submodule originating the log message.
 * @param[in] level      The severity/verbosity level. Values greater
 *                       than zero are warnings and errors. Values less
 *                       than zero are debugging or trace messages.
 * @param[in] message    The message itself.
 */
typedef void (* nmos_logging_callback)(
    NvNmosNodeServer *server,
    const char *categories,
    int level,
    const char *message);

typedef struct _NvNmosAssetConfig NvNmosAssetConfig;
typedef struct _NvNmosReceiverConfig NvNmosReceiverConfig;
typedef struct _NvNmosSenderConfig NvNmosSenderConfig;

/**
 * Defines configuration settings used to create an @ref NvNmosNodeServer.
 * The structure should be zero initialized.
 */
typedef struct _NvNmosNodeConfig
{
    /** Holds the fully-qualified host name, e.g. "nmos-node.local" or
        "nmos-node.example.com". May be null in which case the system host
        name is determined automatically. */
    const char *host_name;
    /** Holds the host IP addresses, e.g. "192.0.2.0" and "198.51.100.0".
        The array's size must be equal to #num_host_addresses. May be null
        in which case the system host addresses are determined
        automatically. */
    const char **host_addresses;
    /** Holds the number of #host_addresses. May be zero. */
    unsigned int num_host_addresses;
    /** Holds the port number for the HTTP APIs, e.g. 80.
        May be zero in which case default ports are used for each API. */
    unsigned int http_port;

    /** Holds the label of the node and device. May be null in which case
        the #asset_tags are used to generate the label. */
    const char* label;
    /** Holds the description of the node and device. May be null in which
        case the #asset_tags are used to generate the description. */
    const char* description;

    /** Holds BCP-002-02 Asset Distinguishing Information. May be null. */
    NvNmosAssetConfig* asset_tags;

    /** Holds a string used to ensure repeatable UUID generation.
        May be null in which case a random seed is used; not recommended. */
    const char *seed;

    /** Holds configuration settings for the receivers. The array's size
        must be equal to #num_receivers. May be null. */
    NvNmosReceiverConfig *receivers;
    /** Holds the number of #receivers. May be zero. */
    unsigned int num_receivers;
    /** Holds configuration settings for the senders. The array's size
        must be equal to #num_senders. May be null. */
    NvNmosSenderConfig *senders;
    /** Holds the number of #senders. May be zero. */
    unsigned int num_senders;

    /** Holds the callback for handling an IS-05 Connection API activation.
        May be null. */
    nmos_connection_rtp_activation_callback rtp_connection_activated;

    /** Holds the callback for handling log messages. May be null. */
    nmos_logging_callback log_callback;
    /** Holds the minimum severity/verbosity level for which to make
        logging callbacks. */
    int log_level;
    /** Holds topics for which to make logging callbacks. The array's size
        must be equal to #num_log_categories. May be null. */
    const char **log_categories;
    /** Holds the number of #log_categories. May be zero. */
    unsigned int num_log_categories;
} NvNmosNodeConfig;

/**
 * Defines asset distinguishing information for BCP-002-02 tags in an
 * @ref NvNmosNodeServer.
 */
typedef struct _NvNmosAssetConfig
{
    /** Holds the manufacturer, e.g. "Acme". Must not be null. */
    const char* manufacturer;
    /** Holds the product name, e.g. "Widget Pro". Must not be null. */
    const char* product;
    /** Holds the instance identifier, e.g. "XYZ123-456789". Must not
        be null. */
    const char* instance_id;
    /** Holds the function or functions, e.g. "Decoder", "Encoder",
        "Converter" or "Analyzer". Must not be null. */
    const char** functions;
    /** Holds the number of #functions. Must not be zero. */
    unsigned int num_functions;
} NvNmosAssetConfig;

/**
 * Defines configuration settings used to create receivers in an
 * @ref NvNmosNodeServer.
 */
typedef struct _NvNmosReceiverConfig
{
    /** Holds the Session Description Protocol data used to configure
        the receiver. Must not be null. The SDP data must be valid
        as per the relevant IETF RFC and SMPTE standards for the
        media format and transport.
        The 'x-nvnmos-id' session-level attribute specifies the unique
        identifier for the receiver.
        The 'x-nvnmos-group-hint' session-level attribute may be used to
        specify a group hint tag for the receiver.
        The 'x-nvnmos-iface-ip' media-level attribute is used to specify
        the interface IP address on which the stream is received. */
    const char *sdp;
} NvNmosReceiverConfig;

/**
 * Defines configuration settings used to create senders in an
 * @ref NvNmosNodeServer.
 */
typedef struct _NvNmosSenderConfig
{
    /** Holds the Session Description Protocol data used to configure
        the sender. Must not be null. The SDP data must be valid
        as per the relevant IETF RFC and SMPTE standards for the
        media format and transport.
        The 'ts-refclk' attributes are used to specify the node clock.
        The 'x-nvnmos-id' session-level attribute specifies the unique
        identifier for the sender.
        The 'x-nvnmos-group-hint' session-level attribute may be used to
        specify a group hint tag for the sender.
        The 'x-nvnmos-src-port' media-level attribute is used to specify
        the source port from which the stream is transmitted. */
    const char *sdp;
} NvNmosSenderConfig;

/**
 * Holds the implementation details of a running NvNmos server.
 * The structure should be zero initialized, with the possible
 * exception of the @p user_data member.
 */
typedef struct _NvNmosNodeServer
{
    /**
     * Holds a pointer to user data, not used by the NvNmos library.
     * Can be used for example to access application-specific data in
     * callbacks from the NvNmos library.
     */
    void *user_data;
    /**
     * Holds an opaque pointer used by the NvNmos library.
     */
    void *impl;
} NvNmosNodeServer;

/**
 * Initialize and start an NMOS Node server according to the specified
 * configuration settings.
 *
 * The server should be deinitialized using @ref destroy_nmos_node_server.
 *
 * @param[in] config Pointer to the configuration settings.
 * @param[in] server Pointer to the server to be initialized.
 * @return Whether the server has been created and successfully started.
 */
NVNMOS_API
bool create_nmos_node_server(
    const NvNmosNodeConfig *config,
    NvNmosNodeServer *server);

/**
 * Stop and deinitialize an NMOS Node server.
 *
 * The server should have been successfully initialized using
 * @ref create_nmos_node_server.
 *
 * @param[in] server Pointer to the server to be deinitialized.
 * @return Whether the server has been successfully stopped and deinitialized.
 */
NVNMOS_API
bool destroy_nmos_node_server(
    NvNmosNodeServer *server);

/**
 * Add an NMOS Receiver to an NMOS Node server according to the
 * specified configuration settings.
 *
 * The receiver may be removed using @ref remove_nmos_receiver_from_node_server.
 *
 * @param[in] server Pointer to the server to update.
 * @param[in] config Pointer to the configuration settings.
 * @return Whether the receiver has been successfully added.
 */
NVNMOS_API
bool add_nmos_receiver_to_node_server(
    NvNmosNodeServer *server,
    const NvNmosReceiverConfig* config);

/**
 * Remove an NMOS Receiver from an NMOS Node server.
 *
 * The receiver may have been adding using @ref create_nmos_node_server
 * or @ref add_nmos_receiver_to_node_server.
 *
 * @param[in] server Pointer to the server to update.
 * @param[in] id     The unique identifier for the receiver to be removed.
 * @return Whether the receiver has been successfully removed.
 */
NVNMOS_API
bool remove_nmos_receiver_from_node_server(
    NvNmosNodeServer *server,
    const char* id);

/**
 * Add an NMOS Sender to an NMOS Node server according to the
 * specified configuration settings.
 *
 * The sender may be removed using @ref remove_nmos_sender_from_node_server.
 *
 * @param[in] server Pointer to the server to update.
 * @param[in] config Pointer to the configuration settings.
 * @return Whether the sender has been successfully added.
 */
NVNMOS_API
bool add_nmos_sender_to_node_server(
    NvNmosNodeServer *server,
    const NvNmosSenderConfig* config);

/**
 * Remove an NMOS Sender from an NMOS Node server.
 *
 * The sender may have been adding using @ref create_nmos_node_server
 * or @ref add_nmos_sender_to_node_server.
 *
 * @param[in] server Pointer to the server to update.
 * @param[in] id     The unique identifier for the sender to be removed.
 * @return Whether the receiver has been successfully removed.
 */
NVNMOS_API
bool remove_nmos_sender_from_node_server(
    NvNmosNodeServer *server,
    const char* id);

/**
 * Update the configuration settings of a sender or receiver.
 *
 * @param[in] server A pointer to the server to be updated.
 * @param[in] id     The unique identifier for the sender or receiver
 *                   to be activated or deactivated.
 * @param[in] sdp    The updated Session Description Protocol data
 *                   for the sender or receiver, or a null pointer when
 *                   the sender or receiver is being deactivated.
 *                   The new data only updates the transport parameters
 *                   of the sender or receiver, not the media format.
 *                   The 'inactive' media-level attribute is used to
 *                   indicate a disabled leg.
 *                   For a sender, the 'ts-refclk' attributes are used
 *                   to specify the node clock.
 *                   The 'x-nvnmos-id' session-level attribute specifies
 *                   the unique identifier for the sender or receiver,
 *                   @p id.
 *                   For a receiver, the 'x-nvnmos-iface-ip' media-level
 *                   attribute is used to specify the interface IP
 *                   address on which the stream is received.
 *                   For a sender, the 'x-nvnmos-src-port' media-level
 *                   attribute is used to specify the source port
 *                   from which the stream is transmitted.
 * @return Whether the update has been successfully applied.
 */
NVNMOS_API
bool nmos_connection_rtp_activate(
    NvNmosNodeServer *server,
    const char *id,
    const char *sdp);

#ifdef __cplusplus
}
#endif

#endif

/** @} */
