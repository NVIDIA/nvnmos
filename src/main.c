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

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include "nvnmos.h"

// example video format
#ifdef VIDEO_JXSV
// ST 2110-22 JPEG XS
#ifndef VIDEO_DESCRIPTION
#define VIDEO_DESCRIPTION "JPEG XS, YCbCr-4:2:2, 10 bit, 1280 x 720, progressive, 59.94 Hz"
#endif
#ifndef VIDEO_ENCODING_PARAMETERS
#define VIDEO_ENCODING_PARAMETERS "jxsv/90000"
#endif
#ifndef VIDEO_BANDWIDTH
#define VIDEO_BANDWIDTH "b=AS:116000\r\n" // transport bit rate (kb/s), approx. 1280 * 720 * 60000/1001 * 2 * 1.05 / 1e3
#endif
#ifndef VIDEO_FORMAT_SPECIFIC_PARAMETERS
#define VIDEO_FORMAT_SPECIFIC_PARAMETERS "packetmode=0; profile=High444.12; level=1k-1; sublevel=Sublev3bpp; sampling=YCbCr-4:2:2; width=1280; height=720; exactframerate=60000/1001; depth=10; colorimetry=BT709; TCS=SDR; RANGE=FULL; SSN=ST2110-22:2019; TP=2110TPN"
#endif
#else
// ST 2110-20
#ifndef VIDEO_DESCRIPTION
#define VIDEO_DESCRIPTION "YCbCr-4:2:2, 10 bit, 1920 x 1080, progressive, 50 Hz"
#endif
#ifndef VIDEO_ENCODING_PARAMETERS
#define VIDEO_ENCODING_PARAMETERS "raw/90000"
#endif
#ifndef VIDEO_BANDWIDTH
#define VIDEO_BANDWIDTH ""
#endif
#ifndef VIDEO_FORMAT_SPECIFIC_PARAMETERS
#define VIDEO_FORMAT_SPECIFIC_PARAMETERS "sampling=YCbCr-4:2:2; width=1920; height=1080; exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN; "
#endif
#endif

// example audio format
// ST 2110-30
#ifndef AUDIO_DESCRIPTION
#define AUDIO_DESCRIPTION "2 ch, 48 kHz, 24 bit"
#endif
#ifndef AUDIO_ENCODING_PARAMETERS
#define AUDIO_ENCODING_PARAMETERS "L24/48000/2"
#endif
#ifndef AUDIO_FORMAT_SPECIFIC_PARAMETERS
#define AUDIO_FORMAT_SPECIFIC_PARAMETERS "channel-order=SMPTE2110.(ST); "
#endif

// example MXL video flow definition parameters
// video/v210 (YCbCr-4:2:2, 10 bit, progressive) is hard-coded
#ifndef MXL_VIDEO_FLOW_ID
#define MXL_VIDEO_FLOW_ID "5ede7baf-9dcf-4b80-9e44-bc0f615633b4"
#endif
#ifndef MXL_VIDEO_DESCRIPTION
#define MXL_VIDEO_DESCRIPTION "YCbCr-4:2:2, 10 bit, 1920 x 1080, progressive, 59.94 Hz"
#endif
#ifndef MXL_VIDEO_GRAIN_RATE_NUM
#define MXL_VIDEO_GRAIN_RATE_NUM 60000
#endif
#ifndef MXL_VIDEO_GRAIN_RATE_DEN
#define MXL_VIDEO_GRAIN_RATE_DEN 1001
#endif
#ifndef MXL_VIDEO_FRAME_WIDTH
#define MXL_VIDEO_FRAME_WIDTH 1920
#endif
#ifndef MXL_VIDEO_FRAME_HEIGHT
#define MXL_VIDEO_FRAME_HEIGHT 1080
#endif
#ifndef MXL_VIDEO_COLORSPACE
#define MXL_VIDEO_COLORSPACE "BT709"
#endif
#ifndef MXL_VIDEO_TRANSFER_CHARACTERISTIC
#define MXL_VIDEO_TRANSFER_CHARACTERISTIC "SDR"
#endif

// example MXL audio flow definition parameters
// audio/float32 at 48 kHz is hard-coded
#ifndef MXL_AUDIO_FLOW_ID
#define MXL_AUDIO_FLOW_ID "92029e8a-fb63-46d7-b2f4-abe2f8dbf083"
#endif
#ifndef MXL_AUDIO_DESCRIPTION
#define MXL_AUDIO_DESCRIPTION "2 ch, 48 kHz, 32 bit"
#endif
#ifndef MXL_AUDIO_CHANNEL_COUNT
#define MXL_AUDIO_CHANNEL_COUNT 2
#endif

// example MXL domain id (UUIDv4); see AMWA BCP-007-03
#ifndef MXL_DOMAIN_ID
#define MXL_DOMAIN_ID "212ba127-f746-43c5-87d4-3962ec7ff284"
#endif

#ifndef CLK_PTP
#define CLK_PTP true
#endif

static void handle_log(
    NvNmosNodeServer *server,
    const char *categories,
    int level,
    const char *message)
{
    printf("%s [%d:%s]\n", message, level, categories);
}

static bool handle_connection_activated(
    NvNmosNodeServer *server,
    const char *id,
    const char *transport_file)
{
    printf("%s %s\n", id, transport_file ? "activated via NMOS" : "deactivated via NMOS");
    if (server->user_data && transport_file) printf("%s\n", transport_file);
    return true;
}

// construct example SDP for video sender or receiver
static bool init_video_sdp(char* sdp, size_t sdp_size, bool sender, const char* id, const char* interface_ip, const char* label, const char* group_hint)
{
    const char* description = VIDEO_DESCRIPTION;
    const char* encoding = VIDEO_ENCODING_PARAMETERS;
    const char* bandwidth = VIDEO_BANDWIDTH;
    const char* format_specific_parameters = VIDEO_FORMAT_SPECIFIC_PARAMETERS;

    const char* multicast_ip = "233.252.0.0"; // MCAST-TEST-NET
    const char* source_ip = "192.0.2.0"; // TEST-NET-1
    int destination_port = 5020;
    int source_port = 5004;
    int payload_type = 96; // conventional
    const char* ts_refclk = CLK_PTP
        ? "a=ts-refclk:ptp=IEEE1588-2008:AC-DE-48-23-45-67-01-9F:42\r\n"
          "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n" // use both to include all parameters required for NMOS
        : "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";

    int ntp = (int)time(0);

    int result = snprintf(sdp,
        sdp_size,
        "v=0\r\n"
        "o=- %d %d IN IP4 %s\r\n"
        "s=%s\r\n"
        "i=%s\r\n" // optional
        "t=0 0\r\n"
        "a=x-nvnmos-id:%s\r\n"
        "a=x-nvnmos-group-hint:%s\r\n" // optional
        "m=video %d RTP/AVP %d\r\n"
        "c=IN IP4 %s/64\r\n"
        "%s"
        "a=source-filter: incl IN IP4 %s %s\r\n" // omit for any-source multicast for receiver
        "a=x-nvnmos-iface-ip:%s\r\n"
        "a=x-nvnmos-src-port:%d\r\n" // not applicable for receiver
        "a=rtpmap:%d %s\r\n"
        "a=fmtp:%d %s\r\n"
        "%s"
        "a=mediaclk:direct=0\r\n",
        ntp,
        ntp,
        interface_ip,
        label,
        description,
        id,
        group_hint,
        destination_port,
        payload_type,
        multicast_ip,
        bandwidth,
        multicast_ip,
        sender ? interface_ip : source_ip,
        interface_ip,
        source_port,
        payload_type,
        encoding,
        payload_type,
        format_specific_parameters,
        sender ? ts_refclk : ""
    );

    return 0 < result && (size_t)result < sdp_size;
}

// construct example SDP for audio sender or receiver
static bool init_audio_sdp(char* sdp, size_t sdp_size, bool sender, const char* id, const char* interface_ip, const char* label, const char* group_hint)
{
    const char* description = AUDIO_DESCRIPTION;
    const char* encoding = AUDIO_ENCODING_PARAMETERS;
    const char* format_specific_parameters = AUDIO_FORMAT_SPECIFIC_PARAMETERS;

    const char* multicast_ip = "233.252.0.1"; // MCAST-TEST-NET
    const char* source_ip = "192.0.2.1"; // TEST-NET-1
    int destination_port = 5030;
    int source_port = 5004;
    int payload_type = 97; // conventional
    const char* ptime = "a=ptime:1\r\n";
    const char* ts_refclk = CLK_PTP
        ? "a=ts-refclk:ptp=IEEE1588-2008:AC-DE-48-23-45-67-01-9F:42\r\n"
          "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n" // use both to include all parameters required for NMOS
        : "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";

    int ntp = (int)time(0);

    int result = snprintf(sdp,
        sdp_size,
        "v=0\r\n"
        "o=- %d %d IN IP4 %s\r\n"
        "s=%s\r\n"
        "i=%s\r\n" // optional
        "t=0 0\r\n"
        "a=x-nvnmos-id:%s\r\n"
        "a=x-nvnmos-group-hint:%s\r\n" // optional
        "m=audio %d RTP/AVP %d\r\n"
        "c=IN IP4 %s/64\r\n"
        "a=source-filter: incl IN IP4 %s %s\r\n" // omitted for any-source multicast for receiver
        "a=x-nvnmos-iface-ip:%s\r\n"
        "a=x-nvnmos-src-port:%d\r\n" // not applicable for receiver
        "a=rtpmap:%d %s\r\n"
        "a=fmtp:%d %s\r\n"
        "%s"
        "%s"
        "a=mediaclk:direct=0\r\n",
        ntp,
        ntp,
        interface_ip,
        label,
        description,
        id,
        group_hint,
        destination_port,
        payload_type,
        multicast_ip,
        multicast_ip,
        sender ? interface_ip : source_ip,
        interface_ip,
        source_port,
        payload_type,
        encoding,
        payload_type,
        format_specific_parameters,
        sender ? ptime : "", // optional for receiver caps
        sender ? ts_refclk : ""
    );

    return 0 < result && (size_t)result < sdp_size;
}

// construct example MXL flow definition JSON for an uncompressed v210 video sender or receiver
// see AMWA BCP-007-03 and the MXL library examples
static bool init_video_flow_def(char* flow_def, size_t flow_def_size, bool sender, const char* id, const char* mxl_domain_id, const char* mxl_flow_id, const char* label, const char* group_hint)
{
    int result = snprintf(flow_def,
        flow_def_size,
        "{\n"
        "  \"x-nvnmos-id\": \"%s\",\n"
        "  \"x-nvnmos-mxl-domain-id\": \"%s\",\n"
        "  \"id\": \"%s\",\n"
        "  \"label\": \"%s\",\n"
        "  \"description\": \"" MXL_VIDEO_DESCRIPTION "\",\n"
        "  \"tags\": { \"urn:x-nmos:tag:grouphint/v1.0\": [ \"%s\" ] },\n"
        "  \"format\": \"urn:x-nmos:format:video\",\n"
        "  \"media_type\": \"video/v210\",\n"
        "  \"grain_rate\": { \"numerator\": %d, \"denominator\": %d },\n"
        "  \"frame_width\": %d,\n"
        "  \"frame_height\": %d,\n"
        "  \"interlace_mode\": \"progressive\",\n"
        "  \"colorspace\": \"" MXL_VIDEO_COLORSPACE "\",\n"
        "  \"transfer_characteristic\": \"" MXL_VIDEO_TRANSFER_CHARACTERISTIC "\",\n"
        "  \"components\": [\n"
        "    { \"name\": \"Y\",  \"width\": %d, \"height\": %d, \"bit_depth\": 10 },\n"
        "    { \"name\": \"Cb\", \"width\": %d, \"height\": %d, \"bit_depth\": 10 },\n"
        "    { \"name\": \"Cr\", \"width\": %d, \"height\": %d, \"bit_depth\": 10 }\n"
        "  ]\n"
        "}\n",
        id,
        mxl_domain_id,
        mxl_flow_id,
        label,
        group_hint,
        MXL_VIDEO_GRAIN_RATE_NUM, MXL_VIDEO_GRAIN_RATE_DEN,
        MXL_VIDEO_FRAME_WIDTH, MXL_VIDEO_FRAME_HEIGHT,
        MXL_VIDEO_FRAME_WIDTH, MXL_VIDEO_FRAME_HEIGHT,
        MXL_VIDEO_FRAME_WIDTH / 2, MXL_VIDEO_FRAME_HEIGHT,
        MXL_VIDEO_FRAME_WIDTH / 2, MXL_VIDEO_FRAME_HEIGHT
    );

    return 0 < result && (size_t)result < flow_def_size;
}

// construct example MXL flow definition JSON for an audio/float32 sender or receiver
static bool init_audio_flow_def(char* flow_def, size_t flow_def_size, bool sender, const char* id, const char* mxl_domain_id, const char* mxl_flow_id, const char* label, const char* group_hint)
{
    int result = snprintf(flow_def,
        flow_def_size,
        "{\n"
        "  \"x-nvnmos-id\": \"%s\",\n"
        "  \"x-nvnmos-mxl-domain-id\": \"%s\",\n"
        "  \"id\": \"%s\",\n"
        "  \"label\": \"%s\",\n"
        "  \"description\": \"" MXL_AUDIO_DESCRIPTION "\",\n"
        "  \"tags\": { \"urn:x-nmos:tag:grouphint/v1.0\": [ \"%s\" ] },\n"
        "  \"format\": \"urn:x-nmos:format:audio\",\n"
        "  \"media_type\": \"audio/float32\",\n"
        "  \"sample_rate\": { \"numerator\": 48000, \"denominator\": 1 },\n"
        "  \"channel_count\": %d,\n"
        "  \"bit_depth\": 32\n"
        "}\n",
        id,
        mxl_domain_id,
        mxl_flow_id,
        label,
        group_hint,
        MXL_AUDIO_CHANNEL_COUNT
    );

    return 0 < result && (size_t)result < flow_def_size;
}

static bool get_continue(void)
{
    printf("Continue ([y]/n)?\n");
    int c = fgetc(stdin);
    bool result = c == '\n' || tolower(c) == 'y';
    while (c != '\n' && c != EOF)
        c = fgetc(stdin);
    return result;
}

static inline void print_id(const char *type, const char *internal_id, const char *value)
{
    printf("  %-8s  %-12s  %s\n", type, internal_id, value);
}

// demonstrate the seed-only ID accessors: pure functions of the
// node seed string and (for sender/receiver) the internal id; useful
// for tooling that wants the NMOS IDs without standing up a server
static void print_expected_ids(const char *seed)
{
    char id[NVNMOS_ID_LEN];

    printf("Expected NMOS IDs (computed from seed):\n");

    {
        const bool success = nmos_make_node_id(seed, id, sizeof id);
        print_id("node", "", success ? id : "<error>");
    }

    static const char *const sender_internal_ids[] = {
        "sink-0", "sink-1", "mxl-sink-0", "mxl-sink-1"
    };
    for (size_t i = 0; i < sizeof sender_internal_ids / sizeof sender_internal_ids[0]; ++i)
    {
        const bool success = nmos_make_sender_id(seed, sender_internal_ids[i], id, sizeof id);
        print_id("sender", sender_internal_ids[i], success ? id : "<error>");
    }

    static const char *const receiver_internal_ids[] = {
        "source-0", "source-1", "mxl-source-0", "mxl-source-1"
    };
    for (size_t i = 0; i < sizeof receiver_internal_ids / sizeof receiver_internal_ids[0]; ++i)
    {
        const bool success = nmos_make_receiver_id(seed, receiver_internal_ids[i], id, sizeof id);
        print_id("receiver", receiver_internal_ids[i], success ? id : "<error>");
    }
}

// demonstrate the server-based ID accessors: read what the running
// node server actually advertises; sender/receiver lookups also act
// as an existence check, so removed resources return false
static void print_actual_ids(const NvNmosNodeServer *server)
{
    char id[NVNMOS_ID_LEN];

    printf("Actual NMOS IDs (queried from server):\n");

    {
        const bool success = nmos_get_node_id(server, id, sizeof id);
        print_id("node", "", success ? id : "<error>");
    }

    static const char *const sender_internal_ids[] = {
        "sink-0", "sink-1", "mxl-sink-0", "mxl-sink-1"
    };
    for (size_t i = 0; i < sizeof sender_internal_ids / sizeof sender_internal_ids[0]; ++i)
    {
        const bool success = nmos_get_sender_id(server, sender_internal_ids[i], id, sizeof id);
        print_id("sender", sender_internal_ids[i], success ? id : "<missing>");
    }

    static const char *const receiver_internal_ids[] = {
        "source-0", "source-1", "mxl-source-0", "mxl-source-1"
    };
    for (size_t i = 0; i < sizeof receiver_internal_ids / sizeof receiver_internal_ids[0]; ++i)
    {
        const bool success = nmos_get_receiver_id(server, receiver_internal_ids[i], id, sizeof id);
        print_id("receiver", receiver_internal_ids[i], success ? id : "<missing>");
    }
}

int main(int argc, char *argv[])
{
    if (argc < 4)
    {
        printf("Usage:\n%s host-name port iface-ip [log-level]\n", argv[0]);
        return 1;
    }

    const char* functions[1] = {
        "Example"
    };

    NvNmosAssetConfig asset_config = { 0 };

    asset_config.manufacturer = "Acme";
    asset_config.product = "Widget Pro";
    asset_config.instance_id = "XYZ123-456789";
    asset_config.functions = &functions[0];
    asset_config.num_functions = 1;

    NvNmosNodeConfig node_config = { 0 };

    node_config.host_name = argv[1];
    node_config.http_port = atoi(argv[2]);
    node_config.asset_tags = &asset_config;

    char seed[512];
    if (snprintf(seed, sizeof seed, "%s:%s", argv[1], argv[2]) < 0) return 1;
    node_config.seed = seed;

    // this example application constructs fairly hard-coded SDP files
    // GstSDPMessage could be used to create the SDP data to configure the NMOS
    // receivers and senders representing the GStreamer sources and sinks

    const char* interface_ip = argv[3];

    char source_sdp[2][2048] = { 0 };
    if (!init_video_sdp(source_sdp[0], sizeof source_sdp[0], false, "source-0", interface_ip, "NvNmos Video Receiver", "rx-0:video")) return 1;
    if (!init_audio_sdp(source_sdp[1], sizeof source_sdp[1], false, "source-1", interface_ip, "NvNmos Audio Receiver", "rx-0:audio")) return 1;

    char sink_sdp[2][2048] = { 0 };
    if (!init_video_sdp(sink_sdp[0], sizeof sink_sdp[0], true, "sink-0", interface_ip, "NvNmos Video Sender", "tx-0:video")) return 1;
    if (!init_audio_sdp(sink_sdp[1], sizeof sink_sdp[1], true, "sink-1", interface_ip, "NvNmos Audio Sender", "tx-0:audio")) return 1;

    // example MXL domain and per-flow ids (just hard-coded UUIDs for this example)
    const char* mxl_domain_id = MXL_DOMAIN_ID;

    char source_mxl[2][2048] = { 0 };
    if (!init_video_flow_def(source_mxl[0], sizeof source_mxl[0], false, "mxl-source-0", mxl_domain_id, MXL_VIDEO_FLOW_ID, "NvNmos MXL Video Receiver", "rx-mxl-0:video")) return 1;
    if (!init_audio_flow_def(source_mxl[1], sizeof source_mxl[1], false, "mxl-source-1", mxl_domain_id, MXL_AUDIO_FLOW_ID, "NvNmos MXL Audio Receiver", "rx-mxl-0:audio")) return 1;

    char sink_mxl[2][2048] = { 0 };
    if (!init_video_flow_def(sink_mxl[0], sizeof sink_mxl[0], true, "mxl-sink-0", mxl_domain_id, MXL_VIDEO_FLOW_ID, "NvNmos MXL Video Sender", "tx-mxl-0:video")) return 1;
    if (!init_audio_flow_def(sink_mxl[1], sizeof sink_mxl[1], true, "mxl-sink-1", mxl_domain_id, MXL_AUDIO_FLOW_ID, "NvNmos MXL Audio Sender", "tx-mxl-0:audio")) return 1;

    NvNmosReceiverConfig source_config[4] = { 0 };

    source_config[0].transport = NVNMOS_TRANSPORT_RTP;
    source_config[0].transport_file = source_sdp[0];
    source_config[1].transport = NVNMOS_TRANSPORT_RTP;
    source_config[1].transport_file = source_sdp[1];
    source_config[2].transport = NVNMOS_TRANSPORT_MXL;
    source_config[2].transport_file = source_mxl[0];
    source_config[3].transport = NVNMOS_TRANSPORT_MXL;
    source_config[3].transport_file = source_mxl[1];

    node_config.receivers = &source_config[0];
    node_config.num_receivers = 4;

    NvNmosSenderConfig sink_config[4] = { 0 };

    sink_config[0].transport = NVNMOS_TRANSPORT_RTP;
    sink_config[0].transport_file = sink_sdp[0];
    sink_config[1].transport = NVNMOS_TRANSPORT_RTP;
    sink_config[1].transport_file = sink_sdp[1];
    sink_config[2].transport = NVNMOS_TRANSPORT_MXL;
    sink_config[2].transport_file = sink_mxl[0];
    sink_config[3].transport = NVNMOS_TRANSPORT_MXL;
    sink_config[3].transport_file = sink_mxl[1];

    node_config.senders = &sink_config[0];
    node_config.num_senders = 4;

    node_config.connection_activated = &handle_connection_activated;

    node_config.log_callback = &handle_log;
    node_config.log_level = argc > 4 ? atoi(argv[4]) : NVNMOS_LOG_ERROR;

    NvNmosNodeServer node_server = { 0 };
    // as an example, use user_data to make handle_connection_activated print the transport file
    node_server.user_data = (void*)1;

    print_expected_ids(seed);

    printf("Creating NvNmos server...\n");
    if (!create_nmos_node_server(&node_config, &node_server)) return 1;

    print_actual_ids(&node_server);

    if (!get_continue()) goto cleanup;
    printf("Removing some senders and receivers...\n");
    if (!remove_nmos_receiver_from_node_server(&node_server, "source-0")) goto cleanup;
    if (!remove_nmos_sender_from_node_server(&node_server, "sink-1")) goto cleanup;
    if (!remove_nmos_receiver_from_node_server(&node_server, "mxl-source-0")) goto cleanup;
    if (!remove_nmos_sender_from_node_server(&node_server, "mxl-sink-1")) goto cleanup;

    print_actual_ids(&node_server);

    if (!get_continue()) goto cleanup;
    printf("Adding back some senders and receivers...\n");
    if (!add_nmos_receiver_to_node_server(&node_server, &source_config[0])) goto cleanup;
    if (!add_nmos_sender_to_node_server(&node_server, &sink_config[1])) goto cleanup;
    if (!add_nmos_receiver_to_node_server(&node_server, &source_config[2])) goto cleanup;
    if (!add_nmos_sender_to_node_server(&node_server, &sink_config[3])) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Activating senders and receivers...\n");
    if (!nmos_connection_activate(&node_server, "source-0", source_config[0].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "source-1", source_config[1].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "sink-0", sink_config[0].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "sink-1", sink_config[1].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-source-0", source_config[2].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-source-1", source_config[3].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-sink-0", sink_config[2].transport_file)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-sink-1", sink_config[3].transport_file)) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Deactivating senders and receivers...\n");
    if (!nmos_connection_activate(&node_server, "source-0", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "source-1", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "sink-0", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "sink-1", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-source-0", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-source-1", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-sink-0", 0)) goto cleanup;
    if (!nmos_connection_activate(&node_server, "mxl-sink-1", 0)) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Destroying NvNmos server...\n");
    if (!destroy_nmos_node_server(&node_server)) return 1;
    printf("Finished\n");
    return 0;

cleanup:
    destroy_nmos_node_server(&node_server);
    return 1;
}
