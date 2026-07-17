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
 * NvNmos currently supports Senders and Receivers for video, audio, and
 * ancillary data flows over RTP/UDP (i.e., SMPTE ST 2110-20, -22, -30,
 * and -40 streams) and over the Media eXchange Layer (MXL).
 *
 * The NvNmos library supports the following specifications, using the <a href="https://github.com/sony/nmos-cpp">Sony nmos-cpp</a> implementation:
 * - <a href="https://specs.amwa.tv/is-04/">AMWA IS-04 NMOS Discovery and Registration Specification</a> v1.3
 * - <a href="https://specs.amwa.tv/is-05/">AMWA IS-05 NMOS Device Connection Management Specification</a> v1.1 and v1.2-dev (for MXL)
 * - <a href="https://specs.amwa.tv/is-09/">AMWA IS-09 NMOS System Parameters Specification</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-002-01/">AMWA BCP-002-01 Natural Grouping of NMOS Resources</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-002-02/">AMWA BCP-002-02 NMOS Asset Distinguishing Information</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-004-01/">AMWA BCP-004-01 NMOS Receiver Capabilities</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-006-01/">AMWA BCP-006-01 NMOS With JPEG XS</a> v1.0
 * - <a href="https://specs.amwa.tv/bcp-007-03/">AMWA BCP-007-03 NMOS With MXL</a> v1.0-dev
 * - Session Description Protocol conforming to SMPTE ST 2110-20, -22, -30, -40, and ST 2022-7
 * - MXL flow definition JSON as consumed by the <a href="https://github.com/dmf-mxl/mxl">MXL SDK</a>
 *
 * @ingroup NvNmosApi
 * @{
 */

#ifndef NVNMOS_H
#define NVNMOS_H

/**
 * @def NVNMOS_API
 * Marks a symbol as part of the public NvNmos library ABI, expanding to the
 * appropriate import, export or visibility attribute for the platform.
 */

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
#include <stddef.h>

#ifdef __cplusplus
extern "C"
{
#endif

/**
 * Buffer size, in bytes, that is always sufficient to hold an NMOS
 * resource id (a UUID in canonical 8-4-4-4-12 hex form) including the
 * terminating null character. Callers may pass buffers of this size
 * or larger to the id accessor functions defined below.
 */
#define NVNMOS_ID_LEN 37

typedef struct _NvNmosNodeServer NvNmosNodeServer;

/**
 * Identifies the transport used by an NvNmos Sender or Receiver. Stored in
 * @ref NvNmosSenderConfig::transport and @ref NvNmosReceiverConfig::transport,
 * it corresponds to the base URN of the transport of the NMOS Sender or
 * Receiver resource.
 */
typedef enum _NvNmosTransport
{
    /** RTP/UDP, as used by SMPTE ST 2110.
        The associated transport file is a Session Description Protocol
        (SDP) file. This is the default for a zero-initialised configuration. */
    NVNMOS_TRANSPORT_RTP = 0,
    /** The Media eXchange Layer (MXL).
        The associated transport file is an MXL flow definition (JSON)
        of the form consumed by the MXL SDK. */
    NVNMOS_TRANSPORT_MXL = 1
} NvNmosTransport;

/**
 * Identifies a resource as either a Sender or a Receiver. Used by
 * @ref nmos_connection_activate and @ref nmos_connection_activation_callback
 * to disambiguate a name that may be shared between a Sender and a
 * Receiver on the same Node (caller-chosen names are unique within a
 * given side on a Node, but a Sender and a Receiver are permitted to
 * use the same name).
 */
typedef enum _NvNmosSide
{
    /** An NMOS Sender. */
    NVNMOS_SIDE_SENDER = 0,
    /** An NMOS Receiver. */
    NVNMOS_SIDE_RECEIVER = 1
} NvNmosSide;

/**
 * Type for a callback from NvNmos library when an IS-05 Connection API
 * activation occurs.
 *
 * @param[in] server         A pointer to the server issuing the callback.
 * @param[in] side           Whether the activation is for a Sender or a
 *                           Receiver.
 * @param[in] name           The caller-chosen name for the sender or
 *                           receiver to be activated or deactivated.
 *                           This is the same name originally supplied
 *                           via the configuration's transport file (see
 *                           @ref NvNmosSenderConfig::transport_file and
 *                           @ref NvNmosReceiverConfig::transport_file).
 *                           Unique for the given @p side on the
 *                           Node.
 * @param[in] transport_file The updated transport file data for the
 *                           sender or receiver, or a null pointer when
 *                           the sender or receiver is being deactivated.
 *
 *                           For an RTP/UDP sender or receiver this is an SDP
 *                           file. The 'inactive' media-level attribute is
 *                           used to indicate a disabled leg. The
 *                           'x-nvnmos-name' session-level attribute specifies
 *                           the name of the sender or receiver, @p name.
 *                           For a receiver, the 'x-nvnmos-iface-ip'
 *                           media-level attribute is used to specify the
 *                           interface IP address on which the stream is
 *                           received. For a sender, the 'x-nvnmos-src-port'
 *                           media-level attribute is used to specify the
 *                           source port from which the stream is
 *                           transmitted.
 *
 *                           For an MXL sender or receiver this is an MXL
 *                           flow definition (JSON). The 'urn:x-nvnmos:tag:name'
 *                           tag in the 'tags' property specifies the name
 *                           of the sender or receiver, @p name;
 *                           the 'mxl_domain_id' and 'mxl_flow_id' IS-05
 *                           transport parameters are reflected in the
 *                           'urn:x-nvnmos:tag:mxl-domain-id' tag (also in
 *                           'tags') and the JSON document's top-level 'id'
 *                           field respectively.
 *                           The application is expected to dispatch on
 *                           (@p side, @p name) (both of which it specified)
 *                           to determine the transport, if needed.
 * @return Whether the activation could be applied.
 */
typedef bool (* nmos_connection_activation_callback)(
    NvNmosNodeServer *server,
    NvNmosSide side,
    const char *name,
    const char *transport_file);

typedef struct _NvNmosChannelMappingActiveMapEntry NvNmosChannelMappingActiveMapEntry;

/**
 * Type for a callback from NvNmos when an IS-08 Channel Mapping API
 * activation completes for one Output.
 *
 * Invoked after nmos-cpp has merged the staged action into
 * `/map/active` for @p output_id. Not invoked for
 * @ref nmos_channelmapping_activate.
 *
 * @param[in] server         The server issuing the callback.
 * @param[in] name           The caller-chosen name of the channel
 *                           mapping (unique per Node). Not IS-08
 *                           `/properties` name.
 * @param[in] output_id      IS-08 output id just activated.
 * @param[in] active_map     Active map for @p output_id; @p num_active_map
 *                           entries, index @c i is output channel @c i.
 *                           Unrouted channels have NULL @p input_id.
 * @param[in] num_active_map Length of @p active_map; must equal that
 *                           output's @p num_channel_labels.
 * @return Whether the data plane applied the active map.
 */
typedef bool (* nmos_channelmapping_activation_callback)(
    NvNmosNodeServer *server,
    const char *name,
    const char *output_id,
    const NvNmosChannelMappingActiveMapEntry *active_map,
    size_t num_active_map);

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
typedef struct _NvNmosNetworkServicesConfig NvNmosNetworkServicesConfig;

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
    nmos_connection_activation_callback connection_activated;

    /** Holds the callback for handling an IS-08 Channel Mapping API
        activation (one Output at a time). May be null. */
    nmos_channelmapping_activation_callback channelmapping_activated;

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

    /** Holds configuration settings for network services to use. May be
        null in which case DNS-SD is used based on the #host_name. */
    NvNmosNetworkServicesConfig* network_services;
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
    /** Holds the transport used by the receiver. Determines the type
        of the @ref transport_file. Defaults to ::NVNMOS_TRANSPORT_RTP
        for a zero-initialised configuration. */
    NvNmosTransport transport;
    /** Holds the transport file data used to configure the receiver.
        Must not be null.

        For ::NVNMOS_TRANSPORT_RTP, this is Session Description Protocol
        (SDP) data, which must be valid as per the relevant IETF RFC
        and SMPTE standards for the media format and transport.
        The 'x-nvnmos-name' session-level attribute specifies the
        caller-chosen name for the receiver, unique within the Node.
        The 'x-nvnmos-group-hint' session-level attribute may be used to
        specify a group hint tag for the receiver.
        The 'x-nvnmos-iface-ip' media-level attribute is used to specify
        the interface IP address on which the stream is received.
        The optional 'x-nvnmos-iface' media-level attribute supplies
        IS-04 interface metadata; otherwise the library attempts to
        resolve the interface IP automatically. The attribute value is:
        <interface-name> [<chassis-id>] <port-id> [<attached-chassis-id>
        <attached-port-id>]
        The 'x-nvnmos-caps' media-level attribute may be used to indicate
        that the receiver should be advertised with the format-derived
        capabilities omitted (i.e. a more permissive receiver). The
        attribute value is: <payload-type>
        The connection address and source filter are not used by the
        receiver itself (since the transport parameters are set
        dynamically by IS-05).

        For ::NVNMOS_TRANSPORT_MXL, this is an MXL flow definition (JSON)
        of the form consumed by the MXL library, with NvNmos extensions
        carried as entries in the 'tags' property.
        The 'urn:x-nvnmos:tag:name' tag (single-string array, required)
        specifies the caller-chosen name for the receiver, unique within
        the Node.
        A group hint tag may be specified via the standard
        'urn:x-nmos:tag:grouphint/v1.0' tag.
        The 'urn:x-nvnmos:tag:caps' tag may be used (presence-only) to
        indicate that the receiver should be advertised with the
        format-derived capabilities omitted.
        The 'urn:x-nvnmos:tag:mxl-domain-id' tag specifies the MXL
        domain for the receiver when a single UUID is specified; when
        the tag is omitted, empty, or [""], the IS-05 mxl_domain_id
        constraint is unconstrained and resolves to null in /active.
        The flow definition's top-level 'id' field is not used by the
        receiver itself (since the MXL flow id is set dynamically by
        IS-05). */
    const char *transport_file;
} NvNmosReceiverConfig;

/**
 * Defines configuration settings used to create senders in an
 * @ref NvNmosNodeServer.
 */
typedef struct _NvNmosSenderConfig
{
    /** Holds the transport used by the sender. Determines the format
        of @ref transport_file. Defaults to ::NVNMOS_TRANSPORT_RTP for
        a zero-initialised configuration. */
    NvNmosTransport transport;
    /** Holds the transport file data used to configure the sender.
        Must not be null.

        For ::NVNMOS_TRANSPORT_RTP, this is Session Description Protocol
        (SDP) data, which must be valid as per the relevant IETF RFC
        and SMPTE standards for the media format and transport.
        The 'ts-refclk' attributes are used to specify the node clock.
        The 'x-nvnmos-name' session-level attribute specifies the
        caller-chosen name for the sender, unique within the Node.
        The 'x-nvnmos-group-hint' session-level attribute may be used to
        specify a group hint tag for the sender.
        The 'x-nvnmos-src-port' media-level attribute is used to specify
        the source port from which the stream is transmitted.
        The 'x-nvnmos-iface-ip' media-level attribute is used to specify
        the interface IP address on which the stream is transmitted.
        The optional 'x-nvnmos-iface' media-level attribute supplies
        IS-04 interface metadata; otherwise the library attempts to
        resolve the interface IP automatically. The attribute value is:
        <interface-name> [<chassis-id>] <port-id> [<attached-chassis-id>
        <attached-port-id>]

        For ::NVNMOS_TRANSPORT_MXL, this is an MXL flow definition (JSON)
        of the form consumed by the MXL library, with NvNmos extensions
        carried as entries in the 'tags' property (following the same
        URN convention as the standard 'urn:x-nmos:tag:grouphint/v1.0').
        The 'urn:x-nvnmos:tag:name' tag (single-string array, required)
        specifies the caller-chosen name for the sender, unique within
        the Node.
        A group hint tag may be specified via the standard
        'urn:x-nmos:tag:grouphint/v1.0' tag.
        The 'urn:x-nvnmos:tag:mxl-domain-id' tag specifies the MXL
        domain for the sender when a single UUID is specified; when
        the tag is omitted, empty, or [""], the IS-05 mxl_domain_id
        constraint is unconstrained and resolves to null in /active.
        The flow definition's top-level 'id' field (UUID string),
        if present, is used as the MXL flow identity for the sender's
        IS-05 transport parameter 'mxl_flow_id'; if absent, the NMOS
        Flow id (derived from @ref NvNmosNodeConfig::seed and the
        'urn:x-nvnmos:tag:name' value) is used in its place. */
    const char *transport_file;
} NvNmosSenderConfig;

/**
 * Defines configuration settings for network services to use in an
 * @ref NvNmosNodeServer. The structure should be zero initialized.
 */
typedef struct _NvNmosNetworkServicesConfig
{
    /** Holds the DNS domain. May be null in which case a domain is
        determined automatically. Use "local" to force multicast DNS-SD. */
    const char* domain;

    /** Holds the IP address or host name of a fixed IS-04 Registration
        API to use; in this case DNS-SD is disabled. May be null in which
        case DNS-SD is used as required by IS-04. */
    const char* registration_address;
    /** Holds the port number for the fixed IS-04 Registration API, if
        #registration_address is specified. May be zero in which case port
        80 is used for HTTP. */
    unsigned int registration_port;
    /** Holds the version number of the fixed IS-04 Registration API, if
        #registration_address is specified. May be null in which case
        "v1.3" is used by default. */
    const char* registration_version;

    /** Holds the IP address or host name of a fixed IS-09 System API
        to use, if #registration_address is also specified. May be null
        in which case a System API is not used; not recommended. */
    const char* system_address;
    /** Holds the port number for the fixed IS-09 System API, if
        #system_address is specified. May be zero in which case port 80
        is used for HTTP. */
    unsigned int system_port;
    /** Holds the version number of the fixed IS-09 System API, if
        #system_address is specified. May be null in which case "v1.0" is
        used by default. */
    const char* system_version;
} NvNmosNetworkServicesConfig;

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
 * @param[in] server        Pointer to the server to update.
 * @param[in] receiver_name The caller-chosen name of the receiver to be
 *                          removed (see @ref NvNmosReceiverConfig::transport_file).
 * @return Whether the receiver has been successfully removed.
 */
NVNMOS_API
bool remove_nmos_receiver_from_node_server(
    NvNmosNodeServer *server,
    const char* receiver_name);

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
 * @param[in] server      Pointer to the server to update.
 * @param[in] sender_name The caller-chosen name of the sender to be
 *                        removed (see @ref NvNmosSenderConfig::transport_file).
 * @return Whether the sender has been successfully removed.
 */
NVNMOS_API
bool remove_nmos_sender_from_node_server(
    NvNmosNodeServer *server,
    const char* sender_name);

/**
 * Report that a sender or receiver has been activated or deactivated
 * out of band.
 *
 * Used when the application's data plane has activated (or deactivated)
 * a sender or receiver by some means other than an IS-05 Connection API
 * patch, so that the IS-04 Node API and IS-05 Connection API model can
 * be updated to reflect the new state. The library does not initiate
 * any activation on the application's behalf.
 *
 * The application's @ref nmos_connection_activation_callback is not
 * invoked as a result of this call.
 *
 * @param[in] server         A pointer to the server to be updated.
 * @param[in] side           Whether the sender or receiver is a Sender
 *                           or a Receiver.
 * @param[in] name           The caller-chosen name of the sender or
 *                           receiver whose state has changed. The
 *                           transport is inferred from the existing
 *                           sender or receiver of @p side with this name.
 * @param[in] transport_file The new transport file data reflecting the
 *                           active state of the sender or receiver, or
 *                           a null pointer when the sender or receiver
 *                           has been deactivated. The new data only
 *                           updates the transport parameters of the
 *                           sender or receiver, not the media format.
 *                           See
 *                           @ref NvNmosSenderConfig::transport_file and
 *                           @ref NvNmosReceiverConfig::transport_file
 *                           for the recognised format (SDP for RTP,
 *                           MXL flow definition JSON for MXL) and the
 *                           supported NvNmos extensions.
 * @return Whether the update has been successfully applied.
 */
NVNMOS_API
bool nmos_connection_activate(
    NvNmosNodeServer *server,
    NvNmosSide side,
    const char *name,
    const char *transport_file);

/**
 * Parent type for an IS-08 Input's /parent endpoint.
 */
typedef enum _NvNmosChannelMappingParentType
{
    /** IS-04 Receiver parent. Default (0). */
    NVNMOS_CHANNELMAPPING_PARENT_TYPE_RECEIVER = 0,
    /** IS-04 Source parent. */
    NVNMOS_CHANNELMAPPING_PARENT_TYPE_SOURCE = 1
} NvNmosChannelMappingParentType;

/**
 * IS-08 Input geometry for @ref add_nmos_channelmapping_to_node_server.
 */
typedef struct _NvNmosChannelMappingInput
{
    /** IS-08 input id; must not be NULL or empty. */
    const char *id;
    /** IS-08 /properties name (UI label); not the caller-chosen channel mapping name. */
    const char *name;
    /** IS-08 /properties description. May be NULL. */
    const char *description;
    /** Per-channel labels; length must equal @p num_channel_labels. */
    const char **channel_labels;
    /** Number of channels; must be non-zero. */
    size_t num_channel_labels;
    /** Caller-chosen receiver or source name for /parent; NULL or "" for null /parent. */
    const char *parent_name;
    /** Receiver vs Source when @p parent_name is set; ignored otherwise. */
    NvNmosChannelMappingParentType parent_type;
    /** Input /caps reordering; defaults to @c true when @p block_size is zero (unset). */
    bool reordering;
    /** Input /caps block_size; defaults to @c 1 when zero (unset). */
    unsigned int block_size;
} NvNmosChannelMappingInput;

/**
 * IS-08 Output geometry for @ref add_nmos_channelmapping_to_node_server.
 */
typedef struct _NvNmosChannelMappingOutput
{
    /** IS-08 output id; must not be NULL or empty. */
    const char *id;
    /** IS-08 /properties name. May be NULL. */
    const char *name;
    /** IS-08 /properties description. May be NULL. */
    const char *description;
    /** Per-channel labels; length must equal @p num_channel_labels. */
    const char **channel_labels;
    /** Number of channels; must be non-zero. */
    size_t num_channel_labels;
    /** Caller-chosen sender name for /sourceid (IS-04 Source id derived internally); NULL or "" for null /sourceid. */
    const char *sender_name;
    /** Output /caps routable_inputs; NULL or zero length leaves /caps unrestricted. */
    const char **routable_inputs;
    /** Length of @p routable_inputs when non-NULL. */
    size_t num_routable_inputs;
} NvNmosChannelMappingOutput;

/**
 * One output channel entry in an IS-08 active map.
 *
 * Array index is the output channel index (0 .. num_channel_labels-1 on that Output).
 */
typedef struct _NvNmosChannelMappingActiveMapEntry
{
    /** IS-08 input id; NULL for an unrouted output channel. */
    const char *input_id;
    /** Input channel index; ignored when @p input_id is NULL. */
    unsigned int input_channel;
} NvNmosChannelMappingActiveMapEntry;

/**
 * Input/output bundle for @ref add_nmos_channelmapping_to_node_server.
 */
typedef struct _NvNmosChannelMappingConfig
{
    /** IS-08 Inputs; length must equal @p num_inputs. */
    const NvNmosChannelMappingInput *inputs;
    /** Number of Inputs. */
    size_t num_inputs;
    /** IS-08 Outputs; length must equal @p num_outputs. */
    const NvNmosChannelMappingOutput *outputs;
    /** Number of Outputs. */
    size_t num_outputs;
} NvNmosChannelMappingConfig;

/**
 * Add channel mapping I/O to a running Node server.
 *
 * Creates real IS-08 Input/Output resources (default unrouted active map).
 *
 * @param[in] server  Pointer to the server.
 * @param[in] name    The caller-chosen name of the channel mapping (unique
 *                    per Node; duplicate names fail).
 * @param[in] mapping Input/output geometry.
 *
 * @return Whether the channel mapping was added. Returns false if @p name is
 *         already in use or any IS-08 input or output id collides with
 *         another channel mapping on the Node.
 */
NVNMOS_API
bool add_nmos_channelmapping_to_node_server(
    NvNmosNodeServer *server,
    const char *name,
    const NvNmosChannelMappingConfig *mapping);

/**
 * Remove a channel mapping and all of its IS-08 I/O.
 *
 * @param[in] server Pointer to the server.
 * @param[in] name   The caller-chosen name of the channel mapping.
 */
NVNMOS_API
bool remove_nmos_channelmapping_from_node_server(
    NvNmosNodeServer *server,
    const char *name);

/**
 * Publish an out-of-band active map for one Output in a channel mapping.
 *
 * Replaces the published active map for @p output_id only. Does not invoke
 * @ref nmos_channelmapping_activation_callback.
 *
 * @param[in] server         Pointer to the server.
 * @param[in] name           The caller-chosen name of the channel
 *                           mapping.
 * @param[in] output_id      IS-08 output id.
 * @param[in] active_map     Active map for @p output_id; @p num_active_map
 *                           entries, index @c i is output channel @c i.
 *                           NULL @p input_id marks an unrouted channel.
 * @param[in] num_active_map Length of @p active_map; must equal that
 *                           output's @p num_channel_labels.
 */
NVNMOS_API
bool nmos_channelmapping_activate(
    NvNmosNodeServer *server,
    const char *name,
    const char *output_id,
    const NvNmosChannelMappingActiveMapEntry *active_map,
    size_t num_active_map);

/**
 * Compute the NMOS Node resource id (the '/self' UUID) that an
 * @ref NvNmosNodeServer created with the given @p seed will use.
 *
 * Pure function of @p seed. The id is generated deterministically by
 * the library, so calling this before @ref create_nmos_node_server
 * yields the same value as @ref nmos_get_node_id on the resulting
 * server. Useful for tooling that needs to pre-compute an id without
 * standing up a server.
 *
 * @param[in]  seed    Seed string. Must not be null. The same string
 *                     used in @ref NvNmosNodeConfig::seed.
 * @param[out] out     Buffer to receive the id as a null-terminated
 *                     ASCII string in canonical UUID form.
 * @param[in]  out_len Size of @p out in bytes. Must be at least
 *                     @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_node_id(
    const char *seed,
    char *out,
    size_t out_len);

/**
 * Compute the NMOS Sender resource id that an
 * @ref NvNmosNodeServer created with the given @p seed will use for
 * the sender with the given @p sender_name.
 *
 * Pure function of (@p seed, @p sender_name). See
 * @ref nmos_make_node_id for the contract; the same notes apply.
 *
 * @param[in]  seed        Seed string. Must not be null.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_sender_id(
    const char *seed,
    const char *sender_name,
    char *out,
    size_t out_len);

/**
 * Compute the NMOS Receiver resource id that an
 * @ref NvNmosNodeServer created with the given @p seed will use for
 * the receiver with the given @p receiver_name.
 *
 * Pure function of (@p seed, @p receiver_name). See
 * @ref nmos_make_node_id for the contract; the same notes apply.
 *
 * @param[in]  seed          Seed string. Must not be null.
 * @param[in]  receiver_name The caller-chosen name of the receiver (see
 *                           @ref NvNmosReceiverConfig::transport_file).
 *                           Must not be null.
 * @param[out] out           Buffer to receive the id.
 * @param[in]  out_len       Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_receiver_id(
    const char *seed,
    const char *receiver_name,
    char *out,
    size_t out_len);

/**
 * Compute the NMOS Source resource id that an @ref NvNmosNodeServer created
 * with the given @p seed will use for the Source paired with a sender
 * named @p sender_name.
 *
 * Pure function of (@p seed, @p sender_name). See @ref nmos_make_node_id
 * for the contract; the same notes apply. This is the IS-04 **Source** id
 * (used for IS-08 `/sourceid`), not the Sender id from
 * @ref nmos_make_sender_id — both are based on the same caller-chosen name
 * used in the transport file.
 *
 * @param[in]  seed        Seed string. Must not be null.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_source_id(
    const char *seed,
    const char *sender_name,
    char *out,
    size_t out_len);

/**
 * Compute the NMOS Flow resource id that an @ref NvNmosNodeServer created
 * with the given @p seed will use for the Flow paired with a sender named
 * @p sender_name.
 *
 * Pure function of (@p seed, @p sender_name). See @ref nmos_make_node_id
 * for the contract; the same notes apply. This is the IS-04 **Flow** id
 * (the Sender's `flow_id` property), not the Sender id from
 * @ref nmos_make_sender_id — both are based on the same caller-chosen name
 * used in the transport file. The MXL `mxl_flow_id` IS-05 transport
 * parameter may be overridden but is the same by default.
 *
 * @param[in]  seed        Seed string. Must not be null.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_flow_id(
    const char *seed,
    const char *sender_name,
    char *out,
    size_t out_len);

/**
 * Get the NMOS Node resource id (the '/self' UUID) of a running
 * @ref NvNmosNodeServer.
 *
 * @param[in]  server  Pointer to a server previously initialised by
 *                     @ref create_nmos_node_server.
 * @param[out] out     Buffer to receive the id.
 * @param[in]  out_len Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_get_node_id(
    const NvNmosNodeServer *server,
    char *out,
    size_t out_len);

/**
 * Get the NMOS Sender resource id of a sender currently registered
 * with the specified server.
 *
 * Looks the sender up by its name. Returns false (without writing to
 * @p out) if no sender with the given @p sender_name has been added
 * to the server.
 *
 * @param[in]  server      Pointer to the server.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_get_sender_id(
    const NvNmosNodeServer *server,
    const char *sender_name,
    char *out,
    size_t out_len);

/**
 * Get the NMOS Receiver resource id of a receiver currently
 * registered with the specified server.
 *
 * Looks the receiver up by its name. Returns false (without writing
 * to @p out) if no receiver with the given @p receiver_name has been
 * added to the server.
 *
 * @param[in]  server        Pointer to the server.
 * @param[in]  receiver_name The caller-chosen name of the receiver (see
 *                           @ref NvNmosReceiverConfig::transport_file).
 *                           Must not be null.
 * @param[out] out           Buffer to receive the id.
 * @param[in]  out_len       Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_get_receiver_id(
    const NvNmosNodeServer *server,
    const char *receiver_name,
    char *out,
    size_t out_len);

/**
 * Get the NMOS Source resource id paired with a sender currently
 * registered with the specified server.
 *
 * Looks the Source up by the caller-chosen sender name (the same
 * string passed to @ref add_nmos_sender_to_node_server). Returns false
 * (without writing to @p out) if no sender with the given @p sender_name
 * has been added to the server.
 *
 * This is the IS-04 **Source** id (used for IS-08 `/sourceid`), not the
 * Sender id from @ref nmos_get_sender_id.
 *
 * @param[in]  server      Pointer to the server.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_get_source_id(
    const NvNmosNodeServer *server,
    const char *sender_name,
    char *out,
    size_t out_len);

/**
 * Get the NMOS Flow resource id paired with a sender currently
 * registered with the specified server.
 *
 * Looks the Flow up by the caller-chosen sender name (the same
 * string passed to @ref add_nmos_sender_to_node_server). Returns false
 * (without writing to @p out) if no sender with the given @p sender_name
 * has been added to the server.
 *
 * This is the IS-04 **Flow** id (the Sender's `flow_id` property), not
 * the Sender id from @ref nmos_get_sender_id. The MXL `mxl_flow_id` IS-05
 * transport parameter may be overridden but is the same by default.
 *
 * @param[in]  server      Pointer to the server.
 * @param[in]  sender_name The caller-chosen name of the sender (see
 *                         @ref NvNmosSenderConfig::transport_file).
 *                         Must not be null.
 * @param[out] out         Buffer to receive the id.
 * @param[in]  out_len     Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_get_flow_id(
    const NvNmosNodeServer *server,
    const char *sender_name,
    char *out,
    size_t out_len);

#ifdef __cplusplus
}
#endif

#endif

/** @} */
