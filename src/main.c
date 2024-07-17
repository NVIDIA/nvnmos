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
#ifndef VIDEO_DESCRIPTION
#define VIDEO_DESCRIPTION "YCbCr-4:2:2, 10 bit, 1920 x 1080, progressive, 50 Hz"
#endif
#ifndef VIDEO_ENCODING_PARAMETERS
#define VIDEO_ENCODING_PARAMETERS "raw/90000"
#endif
#ifndef VIDEO_FORMAT_SPECIFIC_PARAMETERS
#define VIDEO_FORMAT_SPECIFIC_PARAMETERS "sampling=YCbCr-4:2:2; width=1920; height=1080; exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN; "
#endif

// example audio format
#ifndef AUDIO_DESCRIPTION
#define AUDIO_DESCRIPTION "2 ch, 48 kHz, 24 bit"
#endif
#ifndef AUDIO_ENCODING_PARAMETERS
#define AUDIO_ENCODING_PARAMETERS "L24/48000/2"
#endif
#ifndef AUDIO_FORMAT_SPECIFIC_PARAMETERS
#define AUDIO_FORMAT_SPECIFIC_PARAMETERS "channel-order=SMPTE2110.(ST); "
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

static bool handle_rtp_connection_activated(
    NvNmosNodeServer *server,
    const char *id,
    const char *sdp)
{
    printf("%s %s\n", id, sdp ? "activated via NMOS" : "deactivated via NMOS");
    if (server->user_data && sdp) printf("%s\n", sdp);
    return true;
}

// construct example SDP for video sender or receiver
static bool init_video_sdp(char* sdp, size_t sdp_size, bool sender, const char* id, const char* interface_ip, const char* label, const char* group_hint, bool ptp)
{
    const char* description = VIDEO_DESCRIPTION;
    const char* encoding = VIDEO_ENCODING_PARAMETERS;
    const char* format_specific_parameters = VIDEO_FORMAT_SPECIFIC_PARAMETERS;

    const char* multicast_ip = "233.252.0.0"; // MCAST-TEST-NET
    const char* source_ip = "192.0.2.0"; // TEST-NET-1
    int destination_port = 5020;
    int source_port = 5004;
    int payload_type = 96; // conventional
    const char* ts_refclk = ptp
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
static bool init_audio_sdp(char* sdp, size_t sdp_size, bool sender, const char* id, const char* interface_ip, const char* label, const char* group_hint, bool ptp)
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
    const char* ts_refclk = ptp
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

static bool get_continue(void)
{
    printf("Continue ([y]/n)?\n");
    int c = fgetc(stdin);
    bool result = c == '\n' || tolower(c) == 'y';
    while (c != '\n' && c != EOF)
        c = fgetc(stdin);
    return result;
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
    bool ptp = CLK_PTP;

    char source_sdp[2][2048] = { 0 };
    if (!init_video_sdp(source_sdp[0], sizeof source_sdp[0], false, "source-0", interface_ip, "NvNmos Video Receiver", "rx-0:video", ptp)) return 1;
    if (!init_audio_sdp(source_sdp[1], sizeof source_sdp[1], false, "source-1", interface_ip, "NvNmos Audio Receiver", "rx-0:audio", ptp)) return 1;

    char sink_sdp[2][2048] = { 0 };
    if (!init_video_sdp(sink_sdp[0], sizeof sink_sdp[0], true, "sink-0", interface_ip, "NvNmos Video Sender", "tx-0:video", ptp)) return 1;
    if (!init_audio_sdp(sink_sdp[1], sizeof sink_sdp[1], true, "sink-1", interface_ip, "NvNmos Audio Sender", "tx-0:audio", ptp)) return 1;

    NvNmosReceiverConfig source_config[2] = { 0 };

    source_config[0].sdp = source_sdp[0];
    source_config[1].sdp = source_sdp[1];

    node_config.receivers = &source_config[0];
    node_config.num_receivers = 2;

    NvNmosSenderConfig sink_config[2] = { 0 };

    sink_config[0].sdp = sink_sdp[0];
    sink_config[1].sdp = sink_sdp[1];

    node_config.senders = &sink_config[0];
    node_config.num_senders = 2;

    node_config.rtp_connection_activated = &handle_rtp_connection_activated;

    node_config.log_callback = &handle_log;
    node_config.log_level = argc > 4 ? atoi(argv[4]) : NVNMOS_LOG_ERROR;

    NvNmosNodeServer node_server = { 0 };
    // as an example, use user_data to make handle_rtp_connection_activated print the SDP data
    node_server.user_data = (void*)1;

    printf("Creating NvNmos server...\n");
    if (!create_nmos_node_server(&node_config, &node_server)) return 1;
    if (!get_continue()) goto cleanup;
    printf("Removing some senders and receivers...\n");
    if (!remove_nmos_receiver_from_node_server(&node_server, "source-0")) goto cleanup;
    if (!remove_nmos_sender_from_node_server(&node_server, "sink-1")) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Adding back some senders and receivers...\n");
    if (!add_nmos_receiver_to_node_server(&node_server, &source_config[0])) goto cleanup;
    if (!add_nmos_sender_to_node_server(&node_server, &sink_config[1])) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Activating senders and receivers...\n");
    if (!nmos_connection_rtp_activate(&node_server, "source-0", source_config[0].sdp)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "source-1", source_config[1].sdp)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "sink-0", sink_config[0].sdp)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "sink-1", sink_config[1].sdp)) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Deactivating senders and receivers...\n");
    if (!nmos_connection_rtp_activate(&node_server, "source-0", 0)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "source-1", 0)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "sink-0", 0)) goto cleanup;
    if (!nmos_connection_rtp_activate(&node_server, "sink-1", 0)) goto cleanup;
    if (!get_continue()) goto cleanup;
    printf("Destroying NvNmos server...\n");
    if (!destroy_nmos_node_server(&node_server)) return 1;
    printf("Finished\n");
    return 0;

cleanup:
    destroy_nmos_node_server(&node_server);
    return 1;
}
