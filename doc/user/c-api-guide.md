<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# C API Guide

NvNmos consists of a single shared library (`libnvnmos.so` on Linux, `nvnmos.dll` on Windows).
The API is specified by the `nvnmos.h` header file.

See [Core NvNmos Concepts](concepts.md) for the shared transport file,
activation direction, and identity model.

## When to Use the C API

Use the C API when an application owns its data plane and needs to host an
NMOS Node in the application process. Use `nvnmosd` when the NMOS control
plane should run as a separate service, or the GStreamer elements when the
data plane is naturally a GStreamer pipeline.

## Minimal C API Sequence

The application lifecycle is:

1. Zero-initialize `NvNmosNodeConfig` and `NvNmosNodeServer`, then configure
   the Node and callbacks.
2. Call `create_nmos_node_server`.
3. Zero-initialize an `NvNmosSenderConfig` or `NvNmosReceiverConfig`, supply
   its transport and configuring transport file, then call
   `add_nmos_sender_to_node_server` or `add_nmos_receiver_to_node_server`.
4. Run the application data plane and handle `connection_activated` callbacks.
5. Call `destroy_nmos_node_server` during shutdown. Explicitly removing
   Senders and Receivers first is optional.

The
[`nvnmos-example` application](https://github.com/NVIDIA/nvnmos/blob/main/src/main.c)
is the complete example, including configuration, transport files, callbacks,
error handling, and dynamic resource removal and addition.

## Running the Example Application

### Starting the Example Application

Run the nvnmos-example app specifying host name and port, optionally an interface IP, and optionally a log level.

For example:
```sh
nvnmos-example nmos-api.local 8080 192.0.2.0
nvnmos-example nmos-api.local 8080
nvnmos-example nmos-api.local 8080 0
```

The host name can be a .local name, in which case the Node will attempt to discover a Registry being advertised via multicast DNS-SD (mDNS).
When a fully-qualified domain name is specified, e.g. "api.example.com", the NMOS Node will instead use unicast DNS-SD discovery in the relevant domain, e.g. "example.com".

The port is used to serve the HTTP APIs.

The IP address identifies the local interface to be used for the mock RTP/UDP Senders and Receivers.
When omitted, the example uses documentation addresses and also emits `a=x-nvnmos-iface` interface metadata to populate Node interfaces.

The log level ranges between -40 (most verbose) and 40 (least verbose), as per the NvNmos API.
Values greater than zero are warnings and errors. Values less than zero are debugging or trace messages.

The nvnmos-example app runs through the following steps which are output independent of the log level:
```
Creating NvNmos server...
Removing some senders and receivers...
Adding back some senders and receivers...
Activating senders and receivers...
Deactivating senders and receivers...
Destroying NvNmos server...
Finished
```

After each step, the app prompts before moving on to the next step:
```
Continue ([y]/n)?
```

If the app runs successfully to completion, the process exits with code 0.
If any step fails, or the user responds negatively to a prompt, the process exits immediately with code 1.

The example application also creates two MXL Senders and two MXL Receivers (uncompressed `video/v210` and `audio/float32`) and exercises the same add/remove/activate/deactivate cycle for them, alongside the RTP/UDP Senders and Receivers.

### Accessing the NMOS APIs

While the app is running, the IS-04 Node API, the IS-05 Connection API, etc., are available for an NMOS Controller to use.
The HTTP APIs can be accessed at:
```
http://<host-address>:<port>/
```

## Transports

Each `NvNmosSenderConfig` and `NvNmosReceiverConfig` includes a `transport` field (an `NvNmosTransport` enum) that selects the transport. A zero-initialised configuration defaults to RTP. The `transport_file` field then holds the configuring transport file as text in the appropriate format:

| `transport`             | `transport_file` format                                              | Reference  |
| ---                     | ---                                                                  | ---        |
| `NVNMOS_TRANSPORT_RTP`  | Session Description Protocol (SDP) per SMPTE ST 2110 / IETF RFCs     | RFC 4566   |
| `NVNMOS_TRANSPORT_MXL`  | MXL flow definition (JSON) as consumed by the MXL SDK                | MXL SDK    |

NvNmos transport-file extensions and minimal unconstrained Receiver transport files are documented in the [Configuring Transport Files](transport-files.md) guide.

## Connection Activations

The [Activation Direction](concepts.md#activation-direction) guide explains
the two directions in which a connection state change can originate.

### Handling a Controller-Originated Activation

When an IS-05 activation reaches its activation time, the library invokes the
application's `connection_activated` callback with:

- `side`, identifying a Sender or Receiver;
- the caller-chosen `name` for that Sender or Receiver; and
- the effective active `transport_file`, or a null pointer for deactivation.

Dispatch on `(side, name)` to identify the Sender or Receiver and reconfigure
its data plane. Return `true` when the requested state was applied, or `false`
to report failure.

The effective transport file depends on the transport:

- For RTP/UDP, the callback receives the effective active SDP with the IS-05
  `transport_params` applied. For a Receiver, NvNmos uses SDP supplied in the
  IS-05 `PATCH` when present; otherwise it uses the configuring SDP.
- For MXL, the callback receives the configuring MXL flow definition with the
  active `mxl_domain_id` and `mxl_flow_id` in the
  `urn:x-nvnmos:tag:mxl-domain-id` tag and top-level `id`, respectively.

### Reporting an Application-Originated State Change

If the application changes its data plane independently of IS-05, call
`nmos_connection_activate` with the Sender or Receiver's `side`, caller-chosen
`name`, and effective transport file. Pass a null pointer for deactivation.

This updates the IS-04 and IS-05 model. It does not invoke the application's
`connection_activated` callback.

## Troubleshooting

### Address Already in Use

When running multiple NMOS Node instances, each process must be configured to use different ports, i.e., with a unique `port` value.
When the port is already in use, at start-up, the application may show a message like the following:

```
asio listen error: system:98 (Address already in use)
```

### Apple Bonjour Compatibility Warnings

When using Avahi for DNS-SD, shortly after start-up the following lines may be displayed in the log.
They do not indicate a problem and can be ignored.

```
*** WARNING *** The program 'nvnmos-example' uses the Apple Bonjour compatibility layer of Avahi.
*** WARNING *** Please fix your application to use the native API of Avahi!
*** WARNING *** For more information see <http://0pointer.de/blog/projects/avahi-compat.html>
```

### DNSServiceRegister and DNSServiceBrowse Errors

The application may show messages like the following shortly after start-up:

```
DNSServiceRegister reported error: -65537 while registering advertisement for: nmos-cpp_node_192-168-1-194:12345._nmos-node._tcp
DNSServiceBrowse reported error: -65537
```

In this case, the NMOS Node will not be able to discover or register with the NMOS Registry.

One reason for these errors is that the DNS-SD daemon/service is not running.

**Linux**

When using Avahi, check that the `avahi-daemon` is running.

**Windows**

When using mDNSResponder/Bonjour, check that the Bonjour Service is running.
