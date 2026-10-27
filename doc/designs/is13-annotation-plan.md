<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# IS-13 Annotation persistence — plan

Status: implemented
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
| **libnvnmos** | Apply annotations at insert; retain exact label / description reset defaults captured from each resource before annotation; callback for every IS-13 annotation, before the resource is updated | Overlay map, checkpoint file |
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
 * @ref nmos_annotation_update_callback, @ref nmos_make_id and
 * @ref nmos_get_id.
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
 * Whether an IS-13 annotation field is left alone, written, or cleared.
 *
 * #NVNMOS_UPDATE_SET with an empty string is an overlay. #NVNMOS_UPDATE_SET
 * with an empty tag value array is an empty array. #NVNMOS_UPDATE_RESET
 * is JSON null: it clears a label, a description, every writable tag,
 * or one tag key.
 */
typedef enum _NvNmosUpdate
{
    /** Leave the field as it is. */
    NVNMOS_UPDATE_UNCHANGED = 0,
    /** Write the accompanying value. */
    NVNMOS_UPDATE_SET = 1,
    /** Clear the field. */
    NVNMOS_UPDATE_RESET = 2
} NvNmosUpdate;

/**
 * One writable IS-04 tags entry. Read-only tags (including
 * 'urn:x-nvnmos:tag:name' and group hint) are not represented here.
 */
typedef struct _NvNmosTag
{
    /** Holds the tag key. Must not be null or empty. */
    const char *key;
    /** Whether #values is unchanged, written, or cleared. On a
        callback, a key removed by the merge is absent. A key this
        merge left in place is #NVNMOS_UPDATE_UNCHANGED. A key this
        merge wrote is #NVNMOS_UPDATE_SET. */
    NvNmosUpdate values_update;
    /** Holds the tag values. On the callback, a key this merge wrote
        or left in place carries them. At create and on
        @ref nmos_annotation_update they are read when #values_update
        is #NVNMOS_UPDATE_SET. The array's size must be equal to
        #num_values. May be null when #num_values is zero (empty
        array), or when #values_update is not #NVNMOS_UPDATE_SET.
        Must not be null when #num_values is non-zero and
        #values_update is #NVNMOS_UPDATE_SET. Each entry must not be
        null; use "" for an empty string. */
    const char **values;
    /** Holds the number of #values. May be zero. */
    unsigned int num_values;
} NvNmosTag;

/**
 * IS-13 annotation applied at create, carried on
 * @ref nmos_annotation_update_callback, or passed to
 * @ref nmos_annotation_update.
 *
 * At create, #NVNMOS_UPDATE_SET writes the property. #NVNMOS_UPDATE_UNCHANGED
 * leaves the constructed value in place. #NVNMOS_UPDATE_RESET is invalid.
 *
 * On the callback, #label and #description are the effective strings after the
 * merge. #NVNMOS_UPDATE_SET means that string was written as the overlay.
 * #NVNMOS_UPDATE_RESET means the overlay was cleared and the string is the
 * restored default. #NVNMOS_UPDATE_UNCHANGED means the overlay was left alone.
 * #tags is the writable-tag overlay after the merge. A #tags_update of
 * #NVNMOS_UPDATE_SET means the merge wrote the tags object. A key this merge
 * wrote is #NVNMOS_UPDATE_SET. A key this merge left in place is
 * #NVNMOS_UPDATE_UNCHANGED. A key this merge cleared is absent. A #tags_update
 * of #NVNMOS_UPDATE_RESET means every writable tag was cleared and #tags is
 * empty. A #tags_update of #NVNMOS_UPDATE_UNCHANGED means the merge left the
 * tags object alone. #tags still lists the overlay, and each key is
 * #NVNMOS_UPDATE_UNCHANGED.
 *
 * On @ref nmos_annotation_update, only an operation of #NVNMOS_UPDATE_SET or
 * #NVNMOS_UPDATE_RESET is applied, and a value on an unchanged or reset field
 * is ignored. #tags lists the keys to update. A tag whose operation is
 * #NVNMOS_UPDATE_UNCHANGED is ignored.
 */
typedef struct _NvNmosAnnotation
{
    /** Whether #label is unchanged, written, or cleared. */
    NvNmosUpdate label_update;
    /** Holds the label. On the callback this is the effective string. On
        @ref nmos_annotation_update it is read when #label_update is
        #NVNMOS_UPDATE_SET. An empty string is a valid overlay. May be
        null unless #label_update is #NVNMOS_UPDATE_SET. */
    const char *label;
    /** Whether #description is unchanged, written, or cleared. */
    NvNmosUpdate description_update;
    /** Holds the description. On the callback this is the effective
        string. On @ref nmos_annotation_update it is read when
        #description_update is #NVNMOS_UPDATE_SET. An empty string is a
        valid overlay. May be null unless #description_update is
        #NVNMOS_UPDATE_SET. */
    const char *description;
    /** Whether #tags is unchanged, written, or cleared. */
    NvNmosUpdate tags_update;
    /** Holds writable tags. On the callback this is the writable-tag
        overlay after the merge. At create and on
        @ref nmos_annotation_update it lists the keys to update and is
        read when #tags_update is #NVNMOS_UPDATE_SET. A tag whose
        operation is #NVNMOS_UPDATE_UNCHANGED is not read. The array's
        size must be equal to #num_tags. May be null when #num_tags is
        zero, or when #tags_update is not #NVNMOS_UPDATE_SET. Must not
        be null when #num_tags is non-zero and #tags_update is
        #NVNMOS_UPDATE_SET. */
    const NvNmosTag *tags;
    /** Holds the number of #tags. May be zero. */
    unsigned int num_tags;
} NvNmosAnnotation;

/**
 * Type for a callback from NvNmos for an IS-13 annotation update.
 * Called once the annotation has been validated and merged with the
 * resource's current values, before the resource is updated. An HTTP
 * Annotation API PATCH and @ref nmos_annotation_update both invoke it.
 *
 * @param[in] server     The server issuing the callback.
 * @param[in] type       Which IS-04 resource was updated.
 * @param[in] name       The caller-chosen sender or receiver name,
 *                       unique for that side on the Node. Null when
 *                       @p type is ::NVNMOS_RESOURCE_NODE or
 *                       ::NVNMOS_RESOURCE_DEVICE.
 * @param[in] annotation The merged annotation. See #NvNmosAnnotation.
 *                       Must not be null.
 * @return Whether the application accepted the update. When false,
 *         the resource is left unchanged. An Annotation API request
 *         then fails with 500 Internal Error.
 */
typedef bool (* nmos_annotation_update_callback)(
    NvNmosNodeServer *server,
    NvNmosResourceType type,
    const char *name,
    const NvNmosAnnotation *annotation);
```

The header comments are the contract. The callback runs for an HTTP Annotation API PATCH and for `nmos_annotation_update`, on the calling thread, before the resource is updated. Returning false leaves the resource unchanged. An HTTP refusal is 500 Internal Error.

`NvNmosResourceType` is used when the caller already has a type: the annotation callback, and `nmos_make_id` / `nmos_get_id` (§3.4). Create paths keep named config members.

#### 3.1.1 Null, empty, and invalid (`NvNmosAnnotation` / `NvNmosTag`)

`NvNmosUpdate` is the operation performed on each field. `NVNMOS_UPDATE_UNCHANGED` leaves the field alone. `NVNMOS_UPDATE_RESET` is JSON null. `NVNMOS_UPDATE_SET` writes the value, and an empty string or an empty value array is an overlay.

At create, `NVNMOS_UPDATE_SET` writes the property. `NVNMOS_UPDATE_UNCHANGED` leaves the constructed value in place. `NVNMOS_UPDATE_RESET` is invalid. A zero-initialised struct is entirely unchanged, so a label value without `label_update = NVNMOS_UPDATE_SET` is not applied.

The callback reports the merge. The operation on `label` and `description` comes from the PATCH: absent is `NVNMOS_UPDATE_UNCHANGED`, JSON null is `NVNMOS_UPDATE_RESET`, and a string is `NVNMOS_UPDATE_SET`. The value is the effective string after the merge. `NVNMOS_UPDATE_SET` means that string was written as the overlay. `NVNMOS_UPDATE_RESET` means the overlay was cleared and the string is the restored default. `NVNMOS_UPDATE_UNCHANGED` means the overlay was left alone. `tags` is the writable overlay after the merge. A PATCH that omits `tags` still lists those keys, each `NVNMOS_UPDATE_UNCHANGED`, and `tags_update` is `NVNMOS_UPDATE_UNCHANGED`. `"tags": null` is `tags_update = NVNMOS_UPDATE_RESET` and an empty array. A key this PATCH wrote is `NVNMOS_UPDATE_SET`. A key it cleared is absent. There is no per-key reset tombstone.

Create and `nmos_annotation_update` build the same patch. `NVNMOS_UPDATE_UNCHANGED` ignores the member. `NVNMOS_UPDATE_RESET` emits JSON null and ignores a non-null value. `NVNMOS_UPDATE_SET` requires the value. When `tags_update` is `NVNMOS_UPDATE_SET`, `tags` lists the keys to update: a tag `NVNMOS_UPDATE_SET` writes that array, including `[]`, and a tag `NVNMOS_UPDATE_RESET` clears that key. A tag `NVNMOS_UPDATE_UNCHANGED` in that list is ignored. `tags_update` of `NVNMOS_UPDATE_UNCHANGED` or `NVNMOS_UPDATE_RESET` does not read the array. Create rejects a patch that contains JSON null.

IS-13 JSON vs C:

| IS-13 PATCH | Create | Callback | `nmos_annotation_update` |
|-------------|---------|----------|--------------------------|
| omit `label` / `description` | `*_update` unchanged; value ignored | `*_update` unchanged; value is the effective string | `*_update` unchanged |
| `"label": null` | invalid (`NVNMOS_UPDATE_RESET`) | `label_update` reset; value is the restored default | `label_update` reset |
| `"label": ""` | `label_update` set; value `""` | `label_update` set; `label` is `""` | `label_update` set; value `""` |
| `"label": "foo"` | `label_update` set; value `"foo"` | `label_update` set; `label` is `"foo"` | `label_update` set; value `"foo"` |
| omit `tags` | `tags_update` unchanged; `num_tags == 0` | `tags_update` unchanged; the array is still the writable overlay, each key unchanged | `tags_update` unchanged; `num_tags == 0` |
| `"tags": null` | invalid | `tags_update` reset; empty array | `tags_update` reset |
| `"tags": { "foo": null }` | `foo` absent | `tags_update` set; `foo` absent; other writable keys remain | one tag, `values_update` reset, key `foo` |
| `"tags": { "foo": [] }` | one tag, `values_update` set, `num_values == 0` | `tags_update` set; `foo` is set with `num_values == 0`; other writable keys remain | one tag, `values_update` set, `num_values == 0` |
| `"tags": { "foo": ["a"] }` | one tag, `values_update` set, `values[0] == "a"` | `tags_update` set; `foo` is set; other writable keys remain | one tag, `values_update` set, `values[0] == "a"` |

On the callback, the tag array is the writable overlay after the merge, not only the keys named in this PATCH. The application replaces its stored tags when `tags_update` is set, clears them when it is reset, and leaves them when it is unchanged.

| `tags` | `num_tags` | `tags_update` | Meaning | Valid? |
|--------|------------|---------------|---------|--------|
| `NULL` | `0` | unchanged | Omit tags at create and on `nmos_annotation_update`. On the callback, no writable tags remain. | yes |
| `NULL` | `0` | reset | Clear every writable tag. Invalid at create. | update and callback yes; create no |
| `NULL` | `0` | set | Write an empty tag object. At create the empty object leaves the constructed tags in place. | yes |
| non-`NULL` | `0` | unchanged or set | Same as `NULL` / `0`. The callback uses `NULL`. | create and update yes; callback no |
| `NULL` | `> 0` | unchanged | Array is not read. Same as `NULL` / `0` / unchanged. | yes |
| `NULL` | `> 0` | set | | **invalid** |
| non-`NULL` | `> 0` | unchanged | Array is not read. Same as omit tags. The callback uses this for an overlay the PATCH did not update. | yes |
| non-`NULL` | `> 0` | set | That many keys. A set tag writes that array. A reset tag clears that key and is invalid at create. An unchanged tag is not read. On the callback each remaining key is unchanged or set. | yes |
| any | `> 0` | reset | Array is not read. Clear every writable tag. Invalid at create. | update yes; create no |

The `NvNmosAnnotation *` on a config may itself be `NULL` (no annotation at all). The callback's `annotation` argument is never a null pointer. A PATCH that resets every present property still invokes the callback, with those members reset, so the application can delete its entry.

On the callback, each `NvNmosTag` is a writable key that remains. On create and on `nmos_annotation_update`, a tag with `values_update` reset clears that key and the value is ignored; create then rejects the null. A tag with `values_update` unchanged is not read.

| `key` | `values` | `num_values` | Meaning | Valid? |
|-------|----------|--------------|---------|--------|
| `NULL` or `""` | any | any | | **invalid** on a tag that is read |
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

    /** Holds the callback for handling an IS-13 annotation update,
        before the resource is updated; may refuse the change. May be null,
        which leaves the Annotation API unmounted. */
    nmos_annotation_update_callback annotation_updated;
```

On `NvNmosSenderConfig`:

```c
    /** IS-13 annotation for the Source associated with this sender.
        May be null. */
    const NvNmosAnnotation *source_annotation;
    /** IS-13 annotation for the Flow associated with this sender.
        May be null. */
    const NvNmosAnnotation *flow_annotation;
    /** IS-13 annotation for the Sender resource. May be null. */
    const NvNmosAnnotation *sender_annotation;
```

On `NvNmosReceiverConfig`:

```c
    /** IS-13 annotation for the Receiver resource. May be null. */
    const NvNmosAnnotation *receiver_annotation;
```

Create / add / remove signatures stay as they are. Named `nmos_make_*_id` / `nmos_get_*_id` stay as they are; §3.4 adds generic extras beside them.

### 3.3 Callback vs create (same split as Side vs Config)

| Direction | Shape | Why |
|-----------|-------|-----|
| Create | Named members on the config that already owns the name; optional `NvNmosAnnotation *` | Compiler-checked; one AddSender covers source, flow, and sender |
| Callback | `type` + `name` (null for node/device) + the same `NvNmosAnnotation` | One handler, six IS-04 types. `NvNmosUpdate` distinguishes omit, reset, and set without a second struct |

### 3.4 Generic id helpers

Same operation as the named `nmos_make_*_id` / `nmos_get_*_id` functions, with a type argument (like `nmos_connection_activate` taking `NvNmosSide`). Keep every named function; implement them as wrappers around:

```c
/**
 * Compute the NMOS resource id that an @ref NvNmosNodeServer created
 * with the given @p seed will use for the given @p type and @p name.
 *
 * Same contract as the named nmos_make_*_id functions.
 *
 * @param[in]  seed    Seed string. Must not be null.
 * @param[in]  type    Resource type.
 * @param[in]  name    Caller-chosen sender or receiver name. Must be
 *                     null when @p type is ::NVNMOS_RESOURCE_NODE or
 *                     ::NVNMOS_RESOURCE_DEVICE. For Source, Flow and
 *                     Sender this is the sender name; for Receiver
 *                     the receiver name.
 * @param[out] out     Buffer to receive the id.
 * @param[in]  out_len Size of @p out, at least @ref NVNMOS_ID_LEN.
 * @return Whether the id has been written to @p out.
 */
NVNMOS_API
bool nmos_make_id(
    const char *seed,
    NvNmosResourceType type,
    const char *name,
    char *out,
    size_t out_len);

/**
 * Get the NMOS resource id of a resource currently registered with
 * the specified server.
 *
 * Same contract as the named nmos_get_*_id functions. Returns false
 * when the resource is not present.
 */
NVNMOS_API
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

This design does not support default values for writable tags. `"tags": { "foo": null }` removes `foo`; it does not restore a default array. (Name, group hint, and asset tags are read-only, so they are not in this set.) Label and description do have defaults. On the callback, the operation comes from the PATCH, and the value is the effective string on `merged`.

The configuring transport file in `senders` / `receivers` settings is written only at Add and is **not** replaced by IS-05 activation. Activation reads it; `/transportfile` is recomputed each time. That file remains the sender/receiver default for the life of the resource.

Do not reconstruct these defaults in a separate switch over resource types. Capture label and description from the resource after its normal construction and NvNmos-specific overrides, immediately before applying the create annotation. Store that small default object in internal settings keyed by resource id, and erase it when the resource is removed.

### 4.4 Apply at insert

After building a resource from config / transport file (including `set_name`):

1. If the matching `NvNmosAnnotation *` is null, insert as today.
2. Otherwise run `merge_annotation_patch` with `default_value` taken from the resource just built and `patch` built from the struct (`NVNMOS_UPDATE_SET` writes the property; `NVNMOS_UPDATE_UNCHANGED` leaves the constructed value in place; `NVNMOS_UPDATE_RESET` is invalid). Insert the merged resource.

The resource is never advertised with the default and then immediately PATCHed.

`create_nmos_node_server` applies `node_annotation` / `device_annotation` and any annotations on the initial `senders` / `receivers` arrays the same way as later `add_nmos_*`. Invalid annotation members (§3.1.1) fail that add (or fail create if they are on the initial arrays). Apply-at-insert does **not** invoke `annotation_updated`.

### 4.5 Filling the callback (no overlay in the library)

`on_merge_annotation_patch` is `void(const resource&, value& merged, const value& patch)`. `merged` starts as the current `label` / `description` / `tags` and is what will be written back. After `nmos::details::merge_annotation_patch`, reset properties have already been filled with `default_value`, so **`merged` cannot tell a reset from a set** to the same string as the default. The incoming **`patch`** can: key absent = unchanged; JSON `null` = reset; any other value = set.

The library **does not** keep a store-form `NvNmosAnnotation` next to the resource. It builds one callback from this `patch` for the operations, and from `merged` for the label, description, and tag values. Detect presence with `has_field`, not `has_string_field` / `has_object_field`: JSON `null` is not a string or object, and those helpers would treat a reset as omitted.

`patch` is the HTTP body (schema: `label`/`description` are `null` or string; `tags` is `null` or object). The merger does not mutate it. Convert UTF-16 `utility::string_t` to `const char *` the same way as the connection-activation callback; keep those buffers alive until the C function returns. Identity is `resource.type` plus `get_name(resource)` (null for node/device). Call the C callback before the merger returns, still before `modify_resource`.

| PATCH key | `NvNmosUpdate` and members |
|-----------|----------------------------|
| `label` / `description` absent | `*_update` unchanged; value is the effective string |
| `"label": null` (same for `description`) | `*_update` reset; value is the restored default |
| `"label": "…"` | `*_update` set; that string (including when it equals the default) |
| `tags` absent | `tags_update` unchanged; every remaining writable key is unchanged |
| `"tags": null` | `tags_update` reset; empty array |
| `"tags": { … }` | `tags_update` set; every remaining writable key on `merged` (read-only keys omitted). A key this object wrote is set. A key it left in place is unchanged. A key it cleared is absent. |

The operation comes from `patch`. The label and description values come from `merged`, which is the effective string after the merge: the overlay when one remains, and the restored default when the field was reset or has no overlay. Tag values come from `merged` because a tags object is itself a merge-patch, and the writable default is empty, so the post-merge writable set is the overlay.

### 4.6 After IS-13 merge

Invoke `annotation_updated` from the merger with that struct, **before** `modify_resource` commits IS-04. A PATCH that resets every property it contains still invokes the callback so the application can delete its entry.

Do this on the PATCH thread, before the HTTP response, so a concurrent re-Add cannot read a stale application store.

When the callback returns false, the merger throws `std::runtime_error`. nmos-cpp leaves the resource unchanged and responds 500, which IS-13 defines for a request that did not meet the API's additional constraints. nmos-cpp puts the exception message in `debug`; `error` is the default reason phrase.

### 4.7 Remove

`remove_nmos_sender_from_node_server` / `remove_nmos_receiver_from_node_server` do not invoke `annotation_updated`.

## 5. nvnmosd

### 5.1 In-memory store

Flat map, same axes as the daemon's `by_name` index, plus resource type:

```text
(node_seed, NvNmosResourceType, name) -> Annotation
```

`name` is `""` for node and device. `SOURCE`, `FLOW`, and `SENDER` share the sender name as separate keys.

`Annotation` is owned strings: an optional label, an optional description, and a map of writable tag key to values. An absent member is not annotated.

The store is keyed by seed, not by whether a `NvNmosNodeServer` is alive. Destroying the node (session GC, last `CloseSession`) does **not** drop annotations. There is no "forget seed" API in this work.

### 5.2 Pass-through on create

| Daemon call | What to pass into libnvnmos |
|-------------|-----------------------------|
| First `OpenSession` that creates the Node | `node_annotation` / `device_annotation` from `(seed, NODE, "")` and `(seed, DEVICE, "")` |
| Attach to an existing Node | nothing (resources already hold live values) |
| `AddSender` | `source_annotation` / `flow_annotation` / `sender_annotation` for that seed and name, if present |
| `AddReceiver` | `receiver_annotation` for that seed and name, if present |

Lookup is one map get per type.

OpenSession / AddSender / AddReceiver stay as they are; the daemon attaches annotations from the store when it builds `NvNmosNodeConfig` / `NvNmosSenderConfig` / `NvNmosReceiverConfig` for libnvnmos. GStreamer-based apps get that path with no gst-nmos-rs change. An application that needs to update or watch the annotation uses §11, not fields on those requests.

### 5.3 Callback

`annotation_updated`: load or insert the hashmap entry for `(seed, type, name)`, then apply the `NvNmosUpdate` operations:

- `label_update` / `description_update` set: store that string, including `""`. Reset: clear it. Unchanged: leave the stored value.
- `tags_update` set: replace stored writable tags with the callback array. Reset: clear them. Unchanged: leave them. A key the callback lists as unchanged is still part of that replacement array.

If the entry then has nothing annotated, **delete** the key. Otherwise, a new key at the limit (§5.5) is not stored and the callback returns false.

Do this before returning from the libnvnmos callback.

The Annotation API is mounted only when `annotation_updated` is non-null. libnvnmos then leaves `annotation_port` unset and nmos-cpp uses `http_port`. A null callback sets `annotation_port` to `-1`, so the API is not mounted and `/self` does not list the service. Create-time annotations on the config still apply.

IS-05 and IS-08 stay mounted when their callbacks are null. Those callbacks only report activations; the connection resources and channel-mapping resources are still part of the node. IS-13 has no resource of its own.

`NVNMOSD_ANNOTATION_API` defaults on. `0`, `false`, `off`, or `no` disables it for every node in the process. The daemon does not open the checkpoint and does not register `annotation_updated`. The checkpoint variables are unused while it is off.

### 5.4 Checkpoint

One JSON file for the process. On-disk layout (nesting, version field) is an implementation detail; tests treat the file as opaque except that a restart with the same path restores the store.

- Path: `NVNMOSD_ANNOTATION_CHECKPOINT_FILE`. Unset, the file is `<socket-filename>-annotations.json` in the socket's directory (`/tmp/nvnmosd.sock` uses `/tmp/nvnmosd.sock-annotations.json`). Setting the variable to the same path for two daemons will not work: each write replaces the whole file, so they overwrite each other's updates. Tests point the variable at a temp file.
- Load at daemon start, before any `OpenSession`. Missing file: empty store. A file with more entries than the limit (§5.5) is an error. Then replace the file with the loaded store, including when that store is empty, using the same temp-file write. If the existing file cannot be read or parsed, or that replacement fails, the daemon exits. It does not start empty in place of a file it could not load, and it does not keep serving when this start cannot persist. The user deletes a bad file or points the variable at a directory the process can write.
- After a store mutation, debounce (~1 s after the last update; tests may shorten via env) then snapshot: clone the map, compact JSON, write temp, `fsync`, rename. Do not serialize under the state mutex or on the PATCH thread.
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
- **Limit, do not evict.** `NVNMOSD_ANNOTATION_ENTRY_LIMIT` (default 10000). At the limit, log an error and fail the IS-13 PATCH that would add a new entry, while existing annotations keep working and can still be updated or reset. Accepting a PATCH that a restart would lose, or silently dropping a user's label to make room, are both worse than refusing, so no LRU.
- **User path:** the checkpoint is a single JSON file. With the daemon stopped, deleting it clears all annotations and editing it is supported. Document both in the nvnmosd README.

A prune or forget RPC is not part of §11. Add one later if name churn turns out to be real.

### 5.6 Rust wrapper (`nvnmos` crate)

nvnmosd builds `nvnmos::NodeConfig` / `SenderConfig` / `ReceiverConfig` and installs callbacks on `NodeServerBuilder`. It passes stored annotations and receives `annotation_updated`. The callback copies strings out before it returns, as the connection-activation callback does. Add `make_id` / `get_id` next to the existing per-type helpers; do not remove those helpers.

gst-nmos-rs depends on `nvnmos-rpc` only. The gRPC messages it uses do not change.

## 6. nmos-cpp availability (build / CI)

IS-13 is in nmos-cpp ([sony/nmos-cpp#331](https://github.com/sony/nmos-cpp/pull/331)) and in Conan Center `nmos-cpp/cci.20261005`. CI and the Docker image install that package through `src/conan.lock`.

A local nmos-cpp checkout still builds with `nmos_cpp_from_source=True` and `-DUSE_ADD_SUBDIRECTORY=ON`, as in the canary `build-nmos-cpp-from-source` job. That install does not use `src/conan.lock`, which is the packaged graph.

## 7. What this is not

- No nmos-cpp file I/O.
- No annotation fields on OpenSession / AddSender / AddReceiver. Application patch and subscribe are §11.
- No SQLite or other database.
- No gst-nmos-rs change.
- No removal of named `nmos_make_*_id` / `nmos_get_*_id`.

## 8. Implementation order

Landed in this order:

1. Pin Conan Center `nmos-cpp/cci.20261005` (§6).
2. Merger and read-only `urn:x-nvnmos:tag:` tags; `set_name` on source and flow.
3. `NvNmosResourceType` and id helpers.
4. Annotation structs, apply-at-insert, reset, and callback.
5. `nvnmos` crate bindings.
6. nvnmosd store and checkpoint.
7. The probes in §10 were not added. The tests that shipped are §9.

## 9. Permanent tests

IS-13 PATCHes in tests go to the node's Annotation API (HTTP), not gRPC. Discover the port from the running node as other nvnmosd HTTP tests do.

Copy §3.1.1 invalid combinations into add/create tests rather than re-deriving them.

### 9.1 libnvnmos (via `nvnmos` crate or C)

- **Read-only name tag:** PATCH `tags` that omits or replaces `urn:x-nvnmos:tag:name` is rejected; the tag remains. Same for group hint / `urn:x-nvnmos:tag:*`.
- **Name on source and flow:** after AddSender, Source and Flow IS-04 tags include `urn:x-nvnmos:tag:name` equal to the sender name. Callback `name` for `SOURCE` / `FLOW` / `SENDER` is that string; for `NODE` / `DEVICE` it is null.
- **Apply at insert:** AddSender with `sender_annotation.label` set; IS-04 GET shows that label without a prior HTTP PATCH. `annotation_updated` is not invoked.
- **Invalid C struct:** `tags_update` set and `num_tags > 0` with `tags == NULL`; null or empty tag key on a tag that is read; `num_values > 0` with `values == NULL` on a tag that is read; `values[i] == NULL`; duplicate keys among tags that are read; read-only key in a tag that is read; `NVNMOS_UPDATE_RESET` at create — add (or create) fails.
- **Reset vs empty string:** PATCH `"label": null` restores the configuring transport-file session name (sender/receiver) or node config label (node/device); the callback has `label_update` reset and `label` is that restored default. PATCH `"label": ""` leaves empty label; `label_update` is set and `label` is `""`.
- **Annotated value equal to the default:** PATCH label to the same string the configuring file already gives; `label_update` is set and the callback reports that string. Remove, then Add with a transport file carrying a *different* session name plus the stored annotation; GET shows the annotated string, not the new default.
- **One property:** annotate label, then PATCH only `description`; `label_update` is unchanged and `description_update` is set. The callback label is the effective string. The test's stand-in store still has the label. The tag array is still the writable overlay, each key unchanged.
- **One tag:** PATCH `"tags": { "foo": null }` after `foo` was set; `tags_update` is set and the array omits `foo`. A key the PATCH did not write is unchanged. PATCH `"foo": []` reports that key set, with `num_values == 0`. A later PATCH that omits `tags` has `tags_update` unchanged and still lists the remaining keys.
- **Remove+Add same name:** PATCH sender label, `remove_nmos_sender`, `add_nmos_sender` with the annotation the test assembled from flagged callback members; GET shows the PATCHed label. (The test plays the application; the library does not remember the pointer.)
- **Callback lifetime:** pointers are invalid after return (copy in the handler; UAF test only if cheap).
- **Callback timing:** if a test re-Adds from the callback itself, it must see the new annotation (callback runs before the HTTP response).
- **Id helpers:** for every `NvNmosResourceType`, `nmos_make_id` matches the named `nmos_make_*_id`; `nmos_get_id` matches get; node/device reject non-null `name`; missing resource → `nmos_get_id` false.
- **No callback:** a node created without `annotation_updated` does not list `annotation/` and `/self` `services` is empty. A create-time node annotation is still on the IS-04 node.

### 9.2 `nvnmos` crate

Bindings round-trip: Rust `Annotation` → C → create/add → callback copies out equivalent values (null vs `""` vs tags) and the `Update` operations. `make_id` / `get_id` wrappers. `NodeServer::update_annotation` drives `nmos_annotation_update`.

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
- **Limit:** with `NVNMOSD_ANNOTATION_ENTRY_LIMIT` set low, the PATCH past the limit returns 500 and the resource keeps its default; annotations already stored still restore after a restart. A checkpoint with more entries than the limit fails start.
- **Disabled:** `NVNMOSD_ANNOTATION_API=0` starts without reading the checkpoint, including when that file is corrupt. `/x-nmos/` has no `annotation/` and `/self` `services` is empty.

Not required for the persistence work: gst-nmos-rs; gRPC annotation RPCs (§11); `kill -9` durability (accepted loss).

## 10. Probes that were not added

These were only a way to order the work in §8. They are not in the tree. The tests in §9 are the ones that were added.

- No test that PATCHes `tags` with the merger absent and shows `urn:x-nvnmos:tag:name` missing. §9.1 covers the merger keeping that tag.
- §9.1 runs against `nvnmos` and HTTP, with annotations held in the test process. The daemon checkpoint tests are §9.3.
- Checkpoint path, debounce (`NVNMOSD_ANNOTATION_DEBOUNCE_MS`, default 1000), and entry limit are daemon settings, documented in the nvnmosd README, including the checkpoint JSON shape. §9.3 points `NVNMOSD_ANNOTATION_CHECKPOINT_FILE` at a temp file. The entry-limit test checks that a refused key is absent from the file.
- No checkpoint dump helper was added.
- No proto stubs, SQLite spike, gst-nmos-rs hooks, or log-line waits.

## 11. Application access

The daemon remains the store of the annotation. The application does not need to keep a second copy, or discover the Node's HTTP port or open an IS-04 Query API WebSocket to learn about its own resources.

An IS-13 HTTP PATCH and `nmos_annotation_update` use the same libnvnmos merger. That merger decides which tags are read-only, and it rejects an update that names one. This API does not change supported tags, supported values, read-only tags, constraints, or defaults.

### 11.1 `nmos_annotation_update`

`nmos_connection_activate` updates the model and does not invoke `nmos_connection_activation_callback`. That callback asks the application to change its data plane. `nmos_annotation_update_callback` asks the application to keep its copy of the annotation, or to refuse the update. An HTTP IS-13 PATCH and `nmos_annotation_update` both update that copy, so both invoke the callback.

The call takes the model write lock, finds the resource, builds a JSON merge-patch from the `NvNmosUpdate` operations (§3.1.1), and calls the existing annotation merger. The callback therefore sees the report form: a per-key reset is an absent key, not an explicit reset. An annotation whose every operation is unchanged returns success without the callback and without a new IS-04 version.

`NodeServer::update_annotation` returns `Result<()>`, as `activate_connection` does. A `false` from libnvnmos becomes `Error::Failed`.

```c
/**
 * Update the IS-13 annotation of one IS-04 resource.
 *
 * @ref nmos_annotation_update_callback runs on this thread, inside the
 * call, before the resource is updated. An HTTP Annotation API PATCH
 * invokes that same callback.
 *
 * @param[in] server     The server to update.
 * @param[in] type       Which IS-04 resource to update.
 * @param[in] name       The caller-chosen sender or receiver name,
 *                       unique for that side on the Node. Null when
 *                       @p type is ::NVNMOS_RESOURCE_NODE or
 *                       ::NVNMOS_RESOURCE_DEVICE.
 * @param[in] annotation The fields to update. Must not be null.
 *                       #NVNMOS_UPDATE_UNCHANGED leaves a property as
 *                       it is. #NVNMOS_UPDATE_RESET clears it.
 *                       #NVNMOS_UPDATE_SET writes the value. When
 *                       #NvNmosAnnotation::tags_update is
 *                       #NVNMOS_UPDATE_SET, #NvNmosAnnotation::tags
 *                       lists the keys to update, not the whole
 *                       overlay.
 * @return Whether the resource was updated. False indicates the
 *         annotation is invalid, no resource of @p type and @p name
 *         exists, or the callback returned false. The library logs
 *         the reason.
 */
NVNMOS_API
bool nmos_annotation_update(
    NvNmosNodeServer *server,
    NvNmosResourceType type,
    const char *name,
    const NvNmosAnnotation *annotation);
```

### 11.2 gRPC

Two RPCs on `NvnmosDaemon`.

```protobuf
// Which IS-04 resource. `UNSPECIFIED` is the proto3 zero value and is
// an error from a client. The daemon does not send it.
enum ResourceType {
  RESOURCE_TYPE_UNSPECIFIED = 0;
  RESOURCE_TYPE_NODE = 1;
  RESOURCE_TYPE_DEVICE = 2;
  RESOURCE_TYPE_SOURCE = 3;
  RESOURCE_TYPE_FLOW = 4;
  RESOURCE_TYPE_SENDER = 5;
  RESOURCE_TYPE_RECEIVER = 6;
}

// Leave a field alone, write the accompanying value, or clear it.
// `UPDATE_SET` with "" or an empty value list is an overlay.
// `UPDATE_RESET` is JSON null; the accompanying value is ignored.
enum Update {
  UPDATE_UNCHANGED = 0;
  UPDATE_SET = 1;
  UPDATE_RESET = 2;
}

// One writable tag. A read-only key is `INVALID_ARGUMENT`.
message AnnotationTag {
  // Tag key. Empty is `INVALID_ARGUMENT`.
  string key = 1;
  // On `UpdateAnnotation`, `UPDATE_SET` writes `values`, including an
  // empty list, and `UPDATE_RESET` clears the key. `UPDATE_UNCHANGED`
  // is ignored.
  // On `AnnotationEvent` every tag is `UPDATE_UNCHANGED`. The list
  // contains only stored keys, so a cleared key is omitted, it is not
  // indicated by `UPDATE_RESET`.
  Update values_update = 2;
  // Stored values on an event. On `UpdateAnnotation`, applied when
  // `values_update` is `UPDATE_SET`.
  repeated string values = 3;
}

// One annotation. On `UpdateAnnotation` only `UPDATE_SET` and
// `UPDATE_RESET` are applied, and `tags` lists the keys to update.
// On `AnnotationEvent` this is the stored annotation, and `UPDATE_SET`
// is never sent. A present label, description, or tags object is
// `UPDATE_UNCHANGED` with the stored value, including "" and an empty
// value list. A label, description, or whole tags object with no
// overlay is `UPDATE_RESET`. Each tag is `UPDATE_UNCHANGED`, because
// the list contains only stored keys. A cleared key is omitted, it is
// not indicated by `UPDATE_RESET`.
message Annotation {
  Update label_update = 1;
  string label = 2;
  Update description_update = 3;
  string description = 4;
  Update tags_update = 5;
  repeated AnnotationTag tags = 6;
}

// Updates the IS-13 annotation of one resource. Success returns `Empty`.
// The stored annotation is sent on `SubscribeAnnotations`, in the order
// the daemon commits it.
// Returns `FAILED_PRECONDITION` when the Annotation API is off, or when
// the annotation store is full and the resource has no stored annotation.
// Returns `INVALID_ARGUMENT` for `RESOURCE_TYPE_UNSPECIFIED`,
// a malformed tag list, a read-only tag, a `resource_type` that does not
// belong to `resource_handle`, or a non-empty `resource_handle` for Node
// or Device.
// Returns `NOT_FOUND` when the session is unknown or does not own
// `resource_handle`.
// Returns `UNKNOWN` when libnvnmos rejects the update for another reason.
rpc UpdateAnnotation(UpdateAnnotationRequest) returns (Empty);

// Streams the annotations visible to the session. The first events
// are the current annotation of every visible resource, including an
// empty `annotation` where the resource has no stored annotation.
// Later events are the complete annotation whenever it is updated. A
// cleared annotation is an empty `annotation`. There is no
// acknowledgement.
// Adding a sender emits three events (Source, Flow, Sender). Adding a
// receiver emits one. Each is empty when that resource has no stored
// annotation.
// Removing a resource emits nothing.
// Returns `NOT_FOUND` for an unknown session, `ALREADY_EXISTS` if a
// stream is already active, or `FAILED_PRECONDITION` when the
// Annotation API is off.
rpc SubscribeAnnotations(SubscribeAnnotationsRequest) returns (stream AnnotationEvent);

message UpdateAnnotationRequest {
  // Session that owns the resource, or that is attached to the Node
  // when `resource_type` is Node or Device.
  string session_handle = 1;
  // Which IS-04 resource to update. Source, Flow, and Sender refer to
  // the sender identified by `resource_handle`.
  ResourceType resource_type = 2;
  // Opaque handle from `AddSender` or `AddReceiver`. Empty for Node
  // and Device.
  string resource_handle = 3;
  // Fields to update. See `Annotation`.
  Annotation annotation = 4;
}

message SubscribeAnnotationsRequest {
  // Session whose annotations to stream.
  string session_handle = 1;
}

// One complete annotation for a resource the session can see.
message AnnotationEvent {
  // Which IS-04 resource this annotation belongs to.
  ResourceType resource_type = 1;
  // Opaque handle of the sender or receiver. Empty for Node and Device.
  string resource_handle = 2;
  // Stored annotation. The daemon always sets this field. See `Annotation`.
  Annotation annotation = 3;
}
```

### 11.3 Fan-out

One annotation update is sent to every open `SubscribeAnnotations` stream that is allowed to see that resource.

A Node or Device has no owning session. Every session attached to the same node seed shares that Node and Device, and a persistent Node can have no session at all. The session that created a session-refcounted Node is not an owner of it: after that session closes, later sessions are still attached to the same Node. An IS-13 update of the node label is not addressed to one session, so every attached session with the stream open receives the event. A Source, Flow, Sender, or Receiver annotation is sent only to the session that owns that sender or receiver.

Validate `UpdateAnnotation` under the daemon lock, then drop the lock before `NodeServer::update_annotation`. The callback runs during that call. Success returns `Empty`.

After a successful `AnnotationStore::apply`, fan out under the daemon lock. Read the stored entry, or an empty annotation when the entry is gone, and send it to every subscribed session on that node for Node and Device, or only to the owning session otherwise. A session with no annotation stream does not refuse the update. Producing the event is a copy of the store entry. It does not read the live IS-04 resource.

Register a subscriber under the daemon lock before copying its snapshot, and send that snapshot before the RPC returns. Take the daemon lock before the store lock in both the snapshot and the fan-out.

After `commit_add_sender` and `commit_add_receiver`, enqueue the current annotations when that session has a stream. Resource creation applies a stored annotation without the callback, so this emission is separate.

When an event cannot be queued because that session's stream buffer is full, close the stream. The annotation update still succeeds and remains stored. The application calls `SubscribeAnnotations` again, and the new stream begins with a snapshot.

### 11.4 Tests

In the library, drive `nmos_annotation_update` and check that the callback runs inside the call and sees the properties this update wrote, the IS-04 resource is updated, a reset clears it, one tag can be reset without clearing the others, an empty value array can be set, and a read-only tag, malformed tag, or unknown name returns false without updating the resource.

In the daemon, check that a subscribe snapshot includes an event for every visible resource, including an empty annotation, and that adding a sender or receiver emits its events. Check that both an HTTP PATCH and `UpdateAnnotation` emit the stored annotation, that a reset emits an empty annotation, that another session does not see a sender it does not own, and that a Node annotation reaches every subscribed session on that node. Check that an HTTP PATCH still succeeds when nobody is subscribed, that a full buffer closes the stream without failing the update, and that an invalid tag is an invalid-argument status.

No gst-nmos-rs change, and no change to session GC.

### 11.5 Order

On top of current `main` (PR #137 merged):

1. `nmos_annotation_update` (§11.1) and `NodeServer::update_annotation`, returning `Result<()>`.
2. `UpdateAnnotation` and `SubscribeAnnotations`, the snapshot, and the fan-out (§11.2–§11.3).
3. The tests in §11.4.
