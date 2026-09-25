<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# IS-13 Annotation persistence — plan

Status: proposed design
Scope: `nvnmos.h` / libnvnmos, nvnmosd (store and checkpoint). No gst-nmos-rs change is required for GStreamer-based apps to take advantage of this.

nmos-cpp needs **no API change**. Persistence uses the existing `on_merge_annotation_patch` hook and `nmos::details::merge_annotation_patch`.

## 1. Goal

IS-13 PATCHes of `label`, `description`, and writable `tags` survive:

- libnvnmos process restart (nvnmosd checkpoint)
- Remove then Add of the same sender/receiver name in a live session
- `null` in a PATCH restoring the application default (configuring transport file, or node/device config)

Identity in the C API stays **caller-chosen name**, not IS-04 UUID. Node and Device have no name (one Device per Node).

## 2. Layering

| Layer | Owns | Does not own |
|-------|------|----------------|
| **nmos-cpp** | Merge-patch, `null` restore via `default_value`, read-only tag predicate | Files, names, seeds |
| **libnvnmos** | Apply annotations at insert; retain exact label / description reset defaults captured from each resource before annotation; callback after every successful IS-13 merge | Overlay map, checkpoint file |
| **nvnmosd** | Process-wide store, debounce checkpoint, pass stored annotations on OpenSession / AddSender / AddReceiver | IS-13 HTTP |

The public `NvNmos*` types terminate in `nvnmos.cpp`: that adapter validates and converts annotations to nmos-cpp-style JSON before calling `nvnmos_impl.*`, and converts the internal callback back to the C API. The library **does not keep** the `NvNmosAnnotation` pointers it was given. After insert they are gone.

While a resource exists, current values live only in nmos-cpp resource data. libnvnmos retains only the exact default label and description captured from the normally constructed resource before applying its annotation. This avoids a second implementation of resource-default construction without retaining an overlay.

No gst-nmos-rs change is necessary for GStreamer-based apps to take advantage of this. Persistence is entirely a daemon feature.

## 3. `nvnmos.h`

Zero-initialised configs mean "nothing annotated" (same idiom as other optional pointers). Null / empty / invalid rules for members: §3.1.1. Read-only tags never appear in these structs.

### 3.1 Types

```c
/**
 * Identifies an IS-04 resource type. Used by
 * @ref nmos_annotation_callback, @ref nmos_make_id and @ref nmos_get_id.
 * Node and Device have no caller-chosen name (one Device per Node).
 * Source, Flow and Sender share the sender name; Receiver uses the
 * receiver name.
 */
typedef enum _NvNmosResourceType
{
    /** An NMOS Node (IS-04 Node API /self). */
    NVNMOS_RESOURCE_NODE = 0,
    /** An NMOS Device. NvNmos creates one Device per Node. */
    NVNMOS_RESOURCE_DEVICE = 1,
    /** An NMOS Source associated with a Sender. */
    NVNMOS_RESOURCE_SOURCE = 2,
    /** An NMOS Flow associated with a Sender. */
    NVNMOS_RESOURCE_FLOW = 3,
    /** An NMOS Sender. */
    NVNMOS_RESOURCE_SENDER = 4,
    /** An NMOS Receiver. */
    NVNMOS_RESOURCE_RECEIVER = 5
} NvNmosResourceType;

/**
 * One writable IS-04 tags entry. Read-only tags (including
 * 'urn:x-nvnmos:tag:name' and group hint) are not represented here.
 */
typedef struct _NvNmosTag
{
    /** Holds the tag key. Must not be null or empty. */
    const char *key;
    /** Holds the tag values. The array's size must be equal to
        #num_values. May be null when #num_values is zero (empty
        array). Must not be null when #num_values is non-zero.
        Each entry must not be null; use "" for an empty string. */
    const char **values;
    /** Holds the number of #values. May be zero. */
    unsigned int num_values;
} NvNmosTag;

/**
 * IS-13 annotation applied at create, or carried on
 * @ref nmos_annotation_callback for properties that this merge
 * changed.
 *
 * A null #label or #description omits that property at create.
 * On the callback, those members are meaningful only when the
 * matching changed flag is true; then null is a reset and a
 * non-null empty string is an annotated empty value, not a reset.
 */
typedef struct _NvNmosAnnotation
{
    /** Holds the label. May be null. */
    const char *label;
    /** Holds the description. May be null. */
    const char *description;
    /** Holds writable tags. The array's size must be equal to
        #num_tags. May be null when #num_tags is zero. Must not be
        null when #num_tags is non-zero. */
    const NvNmosTag *tags;
    /** Holds the number of #tags. May be zero. */
    unsigned int num_tags;
} NvNmosAnnotation;

/**
 * Type for a callback from NvNmos after a successful IS-13 Annotation
 * API merge.
 *
 * JSON merge-patch; the three bools say which members of @p annotation
 * to apply.
 *
 * Pointers in @p annotation and its members are valid only for the
 * duration of the call. @p annotation is never null.
 *
 * @param[in] server               The server issuing the callback.
 * @param[in] type                 Which IS-04 resource was updated.
 * @param[in] name                 The caller-chosen sender or receiver
 *                                 name, unique for that side on the
 *                                 Node. Null when @p type is
 *                                 ::NVNMOS_RESOURCE_NODE or
 *                                 ::NVNMOS_RESOURCE_DEVICE.
 * @param[in] annotation           Values for properties this merge
 *                                 changed. Must not be null.
 * @param[in] label_changed        True if this merge included
 *                                 'label'. Then a null
 *                                 #NvNmosAnnotation::label is a
 *                                 reset; a string is the overlay.
 * @param[in] description_changed  True if this merge included
 *                                 'description'. Same null/string
 *                                 rule as #label_changed.
 * @param[in] tags_changed         True if this merge included
 *                                 'tags'. Then #NvNmosAnnotation::tags
 *                                 is the full writable-tag overlay
 *                                 after this merge (empty array is
 *                                 a whole-object reset).
 */
typedef void (* nmos_annotation_callback)(
    NvNmosNodeServer *server,
    NvNmosResourceType type,
    const char *name,
    const NvNmosAnnotation *annotation,
    bool label_changed,
    bool description_changed,
    bool tags_changed);
```

`NvNmosResourceType` is used when the caller already has a type: the annotation callback, and `nmos_make_id` / `nmos_get_id` (§3.4). Create paths keep named config members.

#### 3.1.1 Null, empty, and invalid (`NvNmosAnnotation` / `NvNmosTag`)

These structs are **not** a 1:1 encoding of JSON merge-patch. A reset of a property is "omit it" in C (null pointer / not in the `tags` array), never a nested JSON `null` hiding in `values`. The public header can stay short; this table is the test contract.

Create has no changed flags: a null member means omit (leave the application default). The callback uses the same struct plus three `bool`s so omit / reset / set can all be told apart.

IS-13 JSON vs C:

| IS-13 PATCH | Create (`NvNmosAnnotation *`) | Callback |
|-------------|-------------------------------|----------|
| omit `label` / `description` | pointer null (leave default) | matching `*_changed == false`; ignore the member |
| `"label": null` (reset) | (cannot express; create has no reset-as-distinct-from-omit, and does not need to) | `label_changed == true`, `label == NULL` |
| `"label": ""` | pointer to empty string | `label_changed == true`, `label` is `""` |
| `"label": "foo"` | pointer to `"foo"` | `label_changed == true`, `label` is `"foo"` |
| omit `tags` | `tags == NULL && num_tags == 0` | `tags_changed == false`; ignore `tags` |
| `"tags": null` (reset whole object; read-only tags come back) | same empty form (matches omit at create) | `tags_changed == true`, `tags == NULL && num_tags == 0` |
| `"tags": { "foo": null }` (reset one key) | that key **absent** from the `NvNmosTag` array | `tags_changed == true`; the array is the **full** writable overlay after merge, so `foo` is absent |
| `"tags": { "foo": [] }` | one `NvNmosTag` with `key == "foo"`, `num_values == 0` | `tags_changed == true`; that key present with `num_values == 0` (plus any other remaining writable keys) |
| `"tags": { "foo": ["a"] }` | one `NvNmosTag` with `num_values == 1`, `values[0] == "a"` | `tags_changed == true`; full writable overlay including `foo` |

When `tags_changed` is true, the array is not "only keys named in this PATCH". Writable-tag defaults are empty, so the writable tags on the merged resource **are** the overlay. The application replaces its stored tags with that array.

`NvNmosAnnotation` members (create, and callback when the matching flag is true):

| `label` / `description` | Meaning |
|-------------------------|---------|
| `NULL` | Omit at create; **reset** on the callback when `*_changed` |
| `""` | Empty string; this **is** an annotation, not a reset |
| non-empty | That string |

| `tags` | `num_tags` | Meaning | Valid? |
|--------|------------|---------|--------|
| `NULL` | `0` | No writable-tag overrides (omit at create / whole-object reset on the callback). Canonical when `tags_changed`. | yes |
| non-`NULL` | `0` | Same as row above at **create** (zero-length array). Callback **canonicalizes** to `tags == NULL`. | create yes; callback no (library must not emit) |
| `NULL` | `> 0` | | **invalid** (add fails) |
| non-`NULL` | `> 0` | That many writable keys | yes |

The `NvNmosAnnotation *` on a config may itself be `NULL` (no annotation at all). The callback's `annotation` argument is never a null pointer. A PATCH that resets every present key still invokes the callback (`label_changed` / `description_changed` / `tags_changed` true as applicable, members in reset form) so the application can delete its entry.

Each `NvNmosTag` in the array is a **present** writable key with an array value (possibly empty). Reset of that key is omission from the array, not a null `values` pointer.

| `key` | `values` | `num_values` | Meaning | Valid? |
|-------|----------|--------------|---------|--------|
| `NULL` or `""` | any | any | | **invalid** |
| non-empty | `NULL` | `0` | Empty array `[]` | yes |
| non-empty | non-`NULL` | `0` | Empty array `[]`. Callback **canonicalizes** to `values == NULL`. | create yes; callback no |
| non-empty | `NULL` | `> 0` | | **invalid** |
| non-empty | non-`NULL` | `> 0` | That many strings | yes iff every `values[i]` is non-`NULL` |

A `values[i] == NULL` entry is **invalid** (not a per-element reset; use `""` for an empty string in the array). Duplicate `key`s in one `NvNmosAnnotation` are **invalid**.

Read-only keys (`urn:x-nvnmos:tag:name`, group hint, asset tags, …) in `tags` are **invalid** at create (same rejection as an IS-13 PATCH that names them). The callback never lists them.

### 3.2 Config members

On `NvNmosNodeConfig` (in addition to existing `label` / `description` / `asset_tags`, which remain the **defaults**):

```c
    /** IS-13 annotation for the Node resource. May be null. */
    const NvNmosAnnotation *node_annotation;
    /** IS-13 annotation for the Device resource. May be null. */
    const NvNmosAnnotation *device_annotation;
    /** Called after a successful IS-13 merge. May be null. */
    nmos_annotation_callback annotation_changed;
```

On `NvNmosSenderConfig`:

```c
    const NvNmosAnnotation *source_annotation;
    const NvNmosAnnotation *flow_annotation;
    const NvNmosAnnotation *sender_annotation;
```

On `NvNmosReceiverConfig`:

```c
    const NvNmosAnnotation *receiver_annotation;
```

Create / add / remove signatures stay as they are. Named `nmos_make_*_id` / `nmos_get_*_id` stay as they are; §3.4 adds generic extras beside them.

### 3.3 Callback vs create (same split as Side vs Config)

| Direction | Shape | Why |
|-----------|-------|-----|
| Create | Named members on the config that already owns the name; optional `NvNmosAnnotation *` | Compiler-checked; one AddSender covers source, flow, and sender |
| Callback | `type` + `name` (null for node/device) + same `NvNmosAnnotation *` + three `bool`s | One handler, six IS-04 types; merge-patch omit/reset/set without a second struct or a library-held overlay |

### 3.4 Generic id helpers

Same operation as the named `nmos_make_*_id` / `nmos_get_*_id` functions, with a type argument (like `nmos_connection_activate` taking `NvNmosSide`). Keep every named function; implement them as wrappers around:

```c
bool nmos_make_id(
    const char *seed,
    NvNmosResourceType type,
    const char *name,
    char *out,
    size_t out_len);

bool nmos_get_id(
    const NvNmosNodeServer *server,
    NvNmosResourceType type,
    const char *name,
    char *out,
    size_t out_len);
```

`name` contract (same as the named functions, now in one place):

- `NODE` and `DEVICE`: `name` must be null. Non-null is failure (do not hash a name into the node/device id).
- `SOURCE`, `FLOW`, `SENDER`: `name` is the caller-chosen **sender** name (already the contract of `nmos_make_source_id` / `nmos_make_flow_id`).
- `RECEIVER`: `name` is the caller-chosen **receiver** name.

`nmos_get_id` returns false when the resource is not present, matching the existing getters. Unknown `type` returns false.

The annotation store keys on type + name and does not use these helpers.

## 4. libnvnmos

### 4.1 Read-only tags

Wire `on_merge_annotation_patch`. The predicate is nmos-cpp's default (`urn:x-nmos:tag:asset:`, `urn:x-nmos:tag:grouphint/`) **plus** every `urn:x-nvnmos:tag:` key (`name` today; others if they appear on IS-04 tags). A PATCH that names a read-only tag is rejected. After merge, read-only tags are restored.

Today the merger is not installed, so a tags PATCH can drop `urn:x-nvnmos:tag:name`.

### 4.2 Name tag on source and flow

`set_name` already writes `urn:x-nvnmos:tag:name` on Sender and Receiver. Call it on the associated Source and Flow as well (same sender name). Node and Device stay unnamed.

`get_name` on the merged resource then supplies the callback `name` for source, flow, sender, and receiver.

### 4.3 Defaults (IS-13 reset)

`default_value` for `merge_annotation_patch`:

| Resource | Default `label` / `description` | Default writable tags |
|----------|----------------------------------|------------------------|
| Node / Device | `NvNmosNodeConfig` `label` / `description` (and asset-synthesised values when those were null) | none (asset tags are read-only) |
| Sender / Receiver | Configuring transport file: SDP `s=` / `i=`, or MXL `label` / `description` | none (name and group hint are read-only) |
| Source / Flow | nmos-cpp `make_*` defaults from settings (not the transport-file session name / info) | none |

This design does not support default values for writable tags. `"tags": { "foo": null }` removes `foo`; it does not restore a default array. (Name, group hint, and asset tags are read-only, so they are not in this set.) Label and description do have defaults, which is why those callback members come from the PATCH, not from `merged`.

The configuring transport file in `senders` / `receivers` settings is written only at Add and is **not** replaced by IS-05 activation. Activation reads it; `/transportfile` is recomputed each time. That file remains the sender/receiver default for the life of the resource.

Do not reconstruct these defaults in a separate switch over resource types. Capture label and description from the resource after its normal construction and NvNmos-specific overrides, immediately before applying the create annotation. Store that small default object in internal settings keyed by resource id, and erase it when the resource is removed.

### 4.4 Apply at insert

After building a resource from config / transport file (including `set_name`):

1. If the matching `NvNmosAnnotation *` is null, insert as today.
2. Otherwise run `merge_annotation_patch` with `default_value` taken from the resource just built and `patch` built from the struct (omit keys whose pointers are null). Insert the merged resource.

The resource is never advertised with the default and then immediately PATCHed.

`create_nmos_node_server` applies `node_annotation` / `device_annotation` and any annotations on the initial `senders` / `receivers` arrays the same way as later `add_nmos_*`. Invalid annotation members (§3.1.1) fail that add (or fail create if they are on the initial arrays). Apply-at-insert does **not** invoke `annotation_changed`.

### 4.5 Filling the callback (no overlay in the library)

`on_merge_annotation_patch` is `void(const resource&, value& merged, const value& patch)`. `merged` starts as the current `label` / `description` / `tags` and is what will be written back. After `nmos::details::merge_annotation_patch`, reset properties have already been filled with `default_value`, so **`merged` cannot tell a reset from a set** to the same string as the default. The incoming **`patch`** can: key absent = unchanged; JSON `null` = reset; any other value = set.

The library **does not** keep a store-form `NvNmosAnnotation` next to the resource. It builds one callback from this `patch` (and, for tags only, from `merged`). Detect presence with `has_field`, not `has_string_field` / `has_object_field`: JSON `null` is not a string or object, and those helpers would treat a reset as omitted.

`patch` is the HTTP body (schema: `label`/`description` are `null` or string; `tags` is `null` or object). The merger does not mutate it. Convert UTF-16 `utility::string_t` to `const char *` the same way as the connection-activation callback; keep those buffers alive until the C function returns. Identity is `resource.type` plus `get_name(resource)` (null for node/device). Call the C callback before the merger returns, still before `modify_resource`.

| PATCH key | Flags and members |
|-----------|-------------------|
| `label` / `description` absent | `*_changed == false`; that member canonicalized to null |
| `"label": null` (same for `description`) | `*_changed == true`; pointer null (**not** the default string now on `merged`) |
| `"label": "…"` | `*_changed == true`; that string (including when it equals the default) |
| `tags` absent | `tags_changed == false`; `tags` / `num_tags` canonical empty |
| `"tags": null` | `tags_changed == true`; empty writable array |
| `"tags": { … }` | `tags_changed == true`; **all** writable keys on `merged` after the merger (read-only keys omitted). Not only the keys named in this PATCH. |

Label and description values come from `patch`, never from `merged`. Tag values come from `merged` because a tags object is itself a merge-patch, and the writable default is empty, so the post-merge writable set is the overlay.

### 4.6 After IS-13 merge

Invoke `annotation_changed` from the merger with that struct and the three bools, **before** `modify_resource` commits IS-04. Pointers are valid only for the call. A PATCH that resets every key it contains still invokes the callback so the application can delete its entry.

Do this on the PATCH thread, before the HTTP response, so a concurrent re-Add cannot read a stale application store.

### 4.7 Remove

`remove_nmos_sender_from_node_server` / `remove_nmos_receiver_from_node_server` do not invoke `annotation_changed`.

## 5. nvnmosd

### 5.1 In-memory store

Flat map, same axes as the daemon's `by_name` index, plus resource type:

```text
(node_seed, NvNmosResourceType, name) -> Annotation
```

`name` is `""` for node and device. `SOURCE`, `FLOW`, and `SENDER` share the sender name as separate keys.

`Annotation` is owned strings matching `NvNmosAnnotation` (optional label, optional description, map of writable tag key to values). Absent members are reset / at default.

The store is keyed by seed, not by whether a `NvNmosNodeServer` is alive. Destroying the node (session GC, last `CloseSession`) does **not** drop annotations. There is no "forget seed" API in this work.

### 5.2 Pass-through on create

| Daemon call | What to pass into libnvnmos |
|-------------|-----------------------------|
| First `OpenSession` that creates the Node | `node_annotation` / `device_annotation` from `(seed, NODE, "")` and `(seed, DEVICE, "")` |
| Attach to an existing Node | nothing (resources already hold live values) |
| `AddSender` | `source_annotation` / `flow_annotation` / `sender_annotation` for that seed and name, if present |
| `AddReceiver` | `receiver_annotation` for that seed and name, if present |

Lookup is one map get per type.

The gRPC API is **unchanged**. OpenSession / AddSender / AddReceiver stay as they are; the daemon attaches annotations from the store when it builds `NvNmosNodeConfig` / `NvNmosSenderConfig` / `NvNmosReceiverConfig` for libnvnmos. GStreamer-based apps get that path with no gst-nmos-rs change.

Optional follow-up if a gRPC client needs to seed or read the store without IS-13: an `Annotation` message on `NodeConfig` / `AddSenderRequest` / `AddReceiverRequest` (and maybe a get). Not in this work.

### 5.3 Callback

`annotation_changed`: load or insert the hashmap entry for `(seed, type, name)`, then apply **only** flagged members:

- `label_changed`: set or clear stored label (`NULL` → not annotated).
- `description_changed`: same for description.
- `tags_changed`: **replace** stored writable tags with the callback array (empty → no tag overlay).

If the entry then has nothing annotated, **delete** the key.

Do this before returning from the libnvnmos callback.

The Annotation API is mounted only when `annotation_changed` is non-null. libnvnmos then leaves `annotation_port` unset and nmos-cpp uses `http_port`. A null callback sets `annotation_port` to `-1`, so the API is not mounted and `/self` does not list the service. Create-time annotations on the config still apply.

IS-05 and IS-08 stay mounted when their callbacks are null. Those callbacks only report activations; the connection resources and channel-mapping resources are still part of the node. IS-13 has no resource of its own.

`NVNMOSD_ANNOTATION_API` defaults on. `0`, `false`, `off`, or `no` disables it for every node in the process. The daemon does not open the checkpoint and does not register `annotation_changed`. The checkpoint variables are unused while it is off.

### 5.4 Checkpoint

One JSON file for the process. On-disk layout (nesting, version field) is an implementation detail; tests treat the file as opaque except that a restart with the same path restores the store.

- Path: `NVNMOSD_ANNOTATION_CHECKPOINT_FILE`. Unset, the file is `<socket-filename>-annotations.json` in the socket's directory (`/tmp/nvnmosd.sock` uses `/tmp/nvnmosd.sock-annotations.json`). Setting the variable to the same path for two daemons will not work: each write replaces the whole file, so they overwrite each other's updates. Tests point the variable at a temp file.
- Load at daemon start, before any `OpenSession`. Missing file: empty store. Then replace the file with the loaded store, including when that store is empty, using the same temp-file write. If the existing file cannot be read or parsed, or that replacement fails, the daemon exits. It does not start empty in place of a file it could not load, and it does not keep serving when this start cannot persist. The user deletes a bad file or points the variable at a directory the process can write.
- After a store mutation, debounce (~1 s after last change; tests may shorten via env) then snapshot: clone the map, compact JSON, write temp, `fsync`, rename. Do not serialize under the state mutex or on the PATCH thread.
- A checkpoint write that fails after startup is logged. The process keeps running; live sessions are already up.
- Flush on graceful shutdown (so a test that PATCHes then stops the daemon does not need to wait out the debounce).

Debounce limits how often a full rewrite runs. A 20k-entry dump of annotation-sized objects is tens of milliseconds on ordinary hardware.

Crash (`kill -9`) may lose PATCHes still inside the debounce window. That is accepted.

A checkpoint that contains a read-only tag (`urn:x-nmos:tag:asset:`, `urn:x-nmos:tag:grouphint/`, or `urn:x-nvnmos:tag:`) fails startup and names the key. The file is left unchanged.

### 5.5 Bounding the store

Entries are not dropped when a resource is removed or a node destroyed (§5.1), so growth needs its own answer.

- **In-band deletion is IS-13 reset.** Resetting the last annotated property deletes the key (§5.3). That is the only automatic removal.
- **Size is bounded by distinct `(seed, type, name)`, not by uptime**, as long as names are configuration. That is already the contract: a caller-chosen name is unique per side on a Node, and the daemon's `by_name` index assumes it.
- **The failure mode is name churn.** An application that invents a fresh sender name per pipeline run leaves a dead entry per run. Document that names are configured, not generated.
- **Limit, do not evict.** `NVNMOSD_ANNOTATION_ENTRY_LIMIT` (default 10000). At the limit, log an error and refuse new entries while existing annotations keep working. Silently dropping a user's label is worse than refusing to remember a new one, so no LRU.
- **User path:** the checkpoint is a single JSON file. With the daemon stopped, deleting it clears all annotations and editing it is supported. Document both in the nvnmosd README.

A prune or forget RPC belongs with the §5.2 gRPC follow-up, if churn turns out to be real.

### 5.6 Rust wrapper (`nvnmos` crate)

nvnmosd builds `nvnmos::NodeConfig` / `SenderConfig` / `ReceiverConfig` and installs callbacks on `NodeServerBuilder`. It cannot pass stored annotations or receive `annotation_changed` until this crate mirrors the C structs and the three `bool`s (same lifetime rule as the connection-activation callback: copy out before return). Add `make_id` / `get_id` next to the existing per-type helpers; do not remove those helpers.

gst-nmos-rs depends on `nvnmos-rpc` only. The gRPC messages it uses do not change.

## 6. nmos-cpp availability (build / CI)

IS-13 lives in nmos-cpp ([sony/nmos-cpp#331](https://github.com/sony/nmos-cpp/pull/331), `garethsb:rwnode`). `on_merge_annotation_patch` / `nmos::details::merge_annotation_patch` are **not** in Conan Center `nmos-cpp/cci.20260812`.

**Merge this nvnmos work to `main` only after #331 is on sony/nmos-cpp `master`.** Do **not** wait for a new Conan Center package.

Until a CCI revision includes IS-13, libnvnmos builds the same way as today's **Local nmos-cpp Checkout** / canary `build-nmos-cpp-from-source` job:

- `conan install … -o "&:nmos_cpp_from_source=True"` **without** `src/conan.lock` (that lockfile is the packaged nmos-cpp graph)
- sibling `nmos-cpp` clone, `cmake … -DUSE_ADD_SUBDIRECTORY=ON`

Pin the clone to a git SHA, not a floating branch:

| Phase | Clone |
|-------|--------|
| This feature branch, #331 still open | `garethsb/nmos-cpp` at the `rwnode` SHA this work was built against |
| #331 on `master`, CCI still old | `sony/nmos-cpp` at a SHA on `master` that contains IS-13 (retarget CI; merge nvnmos) |
| CCI publishes `nmos-cpp/cci.*` with IS-13 | Restore the packaged require + lockfile as the default; keep the from-source canary |

`ci.yml` on this branch (and on `main` after merge, until CCI) uses that from-source path for the libnvnmos job. The packaged-nmos-cpp canary job is expected to fail compile until CCI catches up; do not make it required on this branch.

## 7. What this is not

- No nmos-cpp file I/O.
- No gRPC / proto annotation fields (optional follow-up; §5.2).
- No SQLite or other database.
- No gst-nmos-rs change.
- No removal of named `nmos_make_*_id` / `nmos_get_*_id`.

## 8. Suggested implementation order

1. Point `ci.yml` at `nmos_cpp_from_source` and a pinned nmos-cpp SHA (§6).
2. Merger + read-only `urn:x-nvnmos:tag:` tags; `set_name` on source and flow (§9.1 can fail on current code).
3. `NvNmosResourceType` and id helpers (§9.2).
4. Annotation structs, apply-at-insert, reset, callback (§9.1 rest).
5. `nvnmos` crate bindings.
6. nvnmosd store + checkpoint (§9.3).
7. Delete §10 probes.

## 9. Permanent tests

IS-13 PATCHes in tests go to the node's Annotation API (HTTP), not gRPC. Discover the port from the running node as other nvnmosd HTTP tests do.

Copy §3.1.1 invalid combinations into add/create tests rather than re-deriving them.

### 9.1 libnvnmos (via `nvnmos` crate or C)

- **Read-only name tag:** PATCH `tags` that omits or replaces `urn:x-nvnmos:tag:name` is rejected; the tag remains. Same for group hint / `urn:x-nvnmos:tag:*`.
- **Name on source and flow:** after AddSender, Source and Flow IS-04 tags include `urn:x-nvnmos:tag:name` equal to the sender name. Callback `name` for `SOURCE` / `FLOW` / `SENDER` is that string; for `NODE` / `DEVICE` it is null.
- **Apply at insert:** AddSender with `sender_annotation.label` set; IS-04 GET shows that label without a prior HTTP PATCH. `annotation_changed` is not invoked.
- **Invalid C struct:** `num_tags > 0` with `tags == NULL`; null tag key; `num_values > 0` with `values == NULL`; `values[i] == NULL`; duplicate keys; read-only key in `tags` — add (or create) fails.
- **Reset vs empty string:** PATCH `"label": null` restores the configuring transport-file session name (sender/receiver) or node config label (node/device); callback has `label_changed == true` and `label == NULL`. PATCH `"label": ""` leaves empty label; `label_changed == true` and `label` is `""`.
- **Annotated value equal to the default:** PATCH label to the same string the configuring file already gives; `label_changed == true` and the callback reports that string, **not** a reset. Remove, then Add with a transport file carrying a *different* session name plus the stored annotation; GET shows the annotated string, not the new default.
- **Partial PATCH:** annotate label, then PATCH only `description`; `label_changed == false`, `description_changed == true`. The test's stand-in store still has the label (it ignored the null member). A PATCH of only `description` on a never-annotated sender has `label_changed == false` (must not treat `label == NULL` as a reset).
- **Partial tags:** PATCH `"tags": { "foo": null }` after `foo` was set; `tags_changed == true` and the array omits `foo` (full writable overlay). PATCH `"foo": []` reports that key with `num_values == 0`. A later PATCH that omits `tags` has `tags_changed == false`.
- **Remove+Add same name:** PATCH sender label, `remove_nmos_sender`, `add_nmos_sender` with the annotation the test assembled from flagged callback members; GET shows the PATCHed label. (The test plays the application; the library does not remember the pointer.)
- **Callback lifetime:** pointers are invalid after return (copy in the handler; UAF test only if cheap).
- **Callback timing:** if a test re-Adds from the callback itself, it must see the new annotation (callback runs before the HTTP response).
- **Id helpers:** for every `NvNmosResourceType`, `nmos_make_id` matches the named `nmos_make_*_id`; `nmos_get_id` matches get; node/device reject non-null `name`; missing resource → `nmos_get_id` false.
- **No callback:** a node created without `annotation_changed` does not list `annotation/` and `/self` `services` is empty. A create-time node annotation is still on the IS-04 node.

### 9.2 `nvnmos` crate

Bindings round-trip: Rust `Annotation` → C → create/add → callback copies out equivalent values (null vs `""` vs tags) and the three changed flags. `make_id` / `get_id` wrappers.

### 9.3 nvnmosd

Checkpoint path pointed at a temp dir. Graceful stop flushes; tests should not sleep the debounce unless they are testing debounce itself.

- **Restart:** PATCH sender (and node) annotations; graceful daemon stop; start with the same file; OpenSession + AddSender (same seed and names, no gRPC annotation fields); IS-04 GET matches the PATCH.
- **Reset then restart:** PATCH `"label": null`; restart; AddSender; GET is the configuring transport-file label (store key gone).
- **Remove+Add without restart:** PATCH; gRPC RemoveSender; AddSender same name; GET still PATCHed (daemon supplies store on Add).
- **Node GC:** PATCH node label; close last session so the `NvNmosNodeServer` is destroyed; OpenSession same seed; GET still PATCHed.
- **Source vs sender:** PATCH source label only; restart; source GET is PATCHed, sender and flow GET stay the transport-file label. PATCH node label only; device GET stays the node-config label.
- **Missing checkpoint:** start with no file; an empty checkpoint is written; nodes use defaults.
- **Corrupt checkpoint:** start fails (does not wipe and continue).
- **Unwritable checkpoint:** the directory cannot accept the replacement write; start fails.
- **Two seeds:** annotations for seed A do not appear on seed B.
- **Limit:** with `NVNMOSD_ANNOTATION_ENTRY_LIMIT` set low, the entry past the limit is refused and logged; annotations already stored still restore after a restart.
- **Disabled:** `NVNMOSD_ANNOTATION_API=0` starts without reading the checkpoint, including when that file is corrupt. `/x-nmos/` has no `annotation/` and `/self` `services` is empty.

Not required: gst-nmos-rs; gRPC annotation fields; `kill -9` durability (accepted loss).

## 10. Temporary infrastructure (delete when the permanent tests exist)

Use these only to order the work in §8. Do not land them.

- **Prove current hole:** one test that PATCHes `tags` without the merger and shows `urn:x-nvnmos:tag:name` missing; then enable §4.1 and invert it into §9.1. Same file, do not keep a "document the bug" test.
- **lib before daemon:** all of §9.1 against `nvnmos` + HTTP, with annotations held in the test process (stand-in for nvnmosd). No checkpoint file yet.
- **Checkpoint file / debounce / entry-limit env:** production knobs, documented in the nvnmosd README. §9.3 points `NVNMOSD_ANNOTATION_CHECKPOINT_FILE` at a temp file.
- **Throwaway:** dump one checkpoint after a PATCH to eyeball JSON; delete the dump helper. Tests treat the file as opaque.
- **Do not:** proto stubs, SQLite spike, gst-nmos-rs hooks, log-line waits for "annotation".
