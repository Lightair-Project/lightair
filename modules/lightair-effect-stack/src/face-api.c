/*
Lightair Effect Stack: the face tracker for others (avatars). Copyright (C) 2026 Bregas S. Wicaksono.

This program is free software; you can redistribute it and/or modify it under the terms of the
GNU General Public License as published by the Free Software Foundation; either version 3 of
the License, or (at your option) any later version.
*/

/* The face tracker, for code outside this module (Lightair's avatars, Auto-frame, the compositor's
 * camera looks), as libobs global procs:
 *
 *   lightair_face_tracker_new(out ptr tracker)
 *   lightair_face_tracker_free(in ptr tracker)
 *   lightair_face_tracker_submit(in ptr tracker, in ptr data, in int linesize, in int width,
 *                                in int height, in int frame_id, in int timestamp_ns, out bool sent)
 *   lightair_face_tracker_get(in ptr tracker, in int frame_id, in ptr face, out bool ok)
 *
 * `data` is a BGRA frame (copied: it may be let go once submit returns); `face` points to an
 * ft_face (face-tracker.h) filled in. `get` may be called from any thread.
 *
 * For a host that tracks and smooths the results itself (Lightair's own compositor drawing the
 * camera looks):
 *
 *   lightair_face_tracker_segmentation(in ptr tracker, in bool enabled, in bool fast,
 *                                      in bool every_frame, in bool person, in int quality)
 *   lightair_face_tracker_get_raw(in ptr tracker, in int serial, in ptr face, out int new_serial,
 *                                 out bool found, out int timestamp_ns, out int width,
 *                                 out int height, out bool ok)
 *   lightair_face_tracker_copy_mask(in ptr tracker, in bool person, in int serial, in ptr buffer,
 *                                   in int capacity, out int new_serial, out int width,
 *                                   out int height, out int timestamp_ns, out bool ok)
 *
 * `get_raw` gives the newest analysed frame's points before smoothing (ok false when nothing is
 * newer than `serial`); `copy_mask` the newest multiclass mask before temporal smoothing (RGBA),
 * or the person mask (one byte a pixel), copied into `buffer` when `capacity` bytes are enough
 * (otherwise ok is false with `width` and `height` set, to retry with a larger buffer).
 *
 * The body and hands, alike: lightair_body_tracker_new/free/submit (no timestamp) and
 * lightair_body_tracker_get(in ptr tracker, in ptr body, out bool ok), `body` a bt_body
 * (body-tracker.h). */

#include <obs-module.h>
#include <callback/proc.h>

#include "face-tracker.h"
#include "body-tracker.h"

static void proc_new(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	obs_enter_graphics();
	face_tracker_set_render_device(gs_get_device_type() == GS_DEVICE_DIRECT3D_11 ? gs_get_device_obj() : NULL);
	obs_leave_graphics();
	char *models = obs_module_file("models");
	face_tracker_t *ft = face_tracker_create(models);
	bfree(models);
	calldata_set_ptr(cd, "tracker", ft);
}

static void proc_free(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	if (ft)
		face_tracker_destroy(ft);
}

static void proc_submit(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	const uint8_t *data = calldata_ptr(cd, "data");
	long long linesize = calldata_int(cd, "linesize");
	long long w = calldata_int(cd, "width"), h = calldata_int(cd, "height");
	bool sent = false;
	if (ft && data && w > 0 && h > 0 && linesize >= w * 4)
		sent = face_tracker_submit_bgra(ft, data, (uint32_t)linesize, (uint32_t)w, (uint32_t)h,
						(uint64_t)calldata_int(cd, "frame_id"),
						(uint64_t)calldata_int(cd, "timestamp_ns"));
	calldata_set_bool(cd, "sent", sent);
}

static void proc_get(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	ft_face *face = calldata_ptr(cd, "face");
	bool ok = ft && face && face_tracker_get_at(ft, (uint64_t)calldata_int(cd, "frame_id"), face);
	calldata_set_bool(cd, "ok", ok);
}

static void proc_segmentation(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	if (!ft)
		return;
	face_tracker_set_segmentation(ft, calldata_bool(cd, "enabled"));
	face_tracker_set_segmentation_fast(ft, calldata_bool(cd, "fast"));
	face_tracker_set_segmentation_every_frame(ft, calldata_bool(cd, "every_frame"));
	face_tracker_set_person(ft, calldata_bool(cd, "person"), (int)calldata_int(cd, "quality"));
}

static void proc_get_raw(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	ft_face *face = calldata_ptr(cd, "face");
	uint64_t serial = (uint64_t)calldata_int(cd, "serial"), ts = 0;
	uint32_t w = 0, h = 0;
	bool found = false;
	bool ok = ft && face && face_tracker_get_raw(ft, &serial, face, &found, &ts, &w, &h);
	calldata_set_int(cd, "new_serial", (long long)serial);
	calldata_set_bool(cd, "found", found);
	calldata_set_int(cd, "timestamp_ns", (long long)ts);
	calldata_set_int(cd, "width", w);
	calldata_set_int(cd, "height", h);
	calldata_set_bool(cd, "ok", ok);
}

static void proc_copy_mask(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	face_tracker_t *ft = calldata_ptr(cd, "tracker");
	uint8_t *buffer = calldata_ptr(cd, "buffer");
	long long capacity = calldata_int(cd, "capacity");
	uint64_t serial = (uint64_t)calldata_int(cd, "serial"), ts = 0;
	uint32_t w = 0, h = 0;
	bool ok = ft && face_tracker_copy_mask(ft, calldata_bool(cd, "person"), &serial, buffer,
					       capacity > 0 ? (uint64_t)capacity : 0, &w, &h, &ts);
	calldata_set_int(cd, "new_serial", (long long)serial);
	calldata_set_int(cd, "width", w);
	calldata_set_int(cd, "height", h);
	calldata_set_int(cd, "timestamp_ns", (long long)ts);
	calldata_set_bool(cd, "ok", ok);
}

static void body_new(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	char *models = obs_module_file("models");
	calldata_set_ptr(cd, "tracker", body_tracker_create(models));
	bfree(models);
}

static void body_free(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	body_tracker_destroy(calldata_ptr(cd, "tracker"));
}

static void body_submit(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	body_tracker_t *bt = calldata_ptr(cd, "tracker");
	const uint8_t *data = calldata_ptr(cd, "data");
	long long linesize = calldata_int(cd, "linesize");
	long long w = calldata_int(cd, "width"), h = calldata_int(cd, "height");
	bool sent = bt && data && w > 0 && h > 0 && linesize >= w * 4 &&
		    body_tracker_submit_bgra(bt, data, (uint32_t)linesize, (uint32_t)w, (uint32_t)h,
					     (uint64_t)calldata_int(cd, "frame_id"));
	calldata_set_bool(cd, "sent", sent);
}

static void body_get(void *unused, calldata_t *cd)
{
	UNUSED_PARAMETER(unused);
	bool ok = body_tracker_get(calldata_ptr(cd, "tracker"), calldata_ptr(cd, "body"));
	calldata_set_bool(cd, "ok", ok);
}

void face_api_register(void)
{
	proc_handler_t *ph = obs_get_proc_handler();
	proc_handler_add(ph, "void lightair_face_tracker_new(out ptr tracker)", proc_new, NULL);
	proc_handler_add(ph, "void lightair_face_tracker_free(in ptr tracker)", proc_free, NULL);
	proc_handler_add(ph,
			 "void lightair_face_tracker_submit(in ptr tracker, in ptr data, in int linesize, in int width, "
			 "in int height, in int frame_id, in int timestamp_ns, out bool sent)",
			 proc_submit, NULL);
	proc_handler_add(ph, "void lightair_face_tracker_get(in ptr tracker, in int frame_id, in ptr face, out bool ok)",
			 proc_get, NULL);
	proc_handler_add(ph,
			 "void lightair_face_tracker_segmentation(in ptr tracker, in bool enabled, in bool fast, "
			 "in bool every_frame, in bool person, in int quality)",
			 proc_segmentation, NULL);
	proc_handler_add(ph,
			 "void lightair_face_tracker_get_raw(in ptr tracker, in int serial, in ptr face, out int new_serial, "
			 "out bool found, out int timestamp_ns, out int width, out int height, out bool ok)",
			 proc_get_raw, NULL);
	proc_handler_add(ph,
			 "void lightair_face_tracker_copy_mask(in ptr tracker, in bool person, in int serial, in ptr buffer, "
			 "in int capacity, out int new_serial, out int width, out int height, out int timestamp_ns, "
			 "out bool ok)",
			 proc_copy_mask, NULL);
	proc_handler_add(ph, "void lightair_body_tracker_new(out ptr tracker)", body_new, NULL);
	proc_handler_add(ph, "void lightair_body_tracker_free(in ptr tracker)", body_free, NULL);
	proc_handler_add(ph,
			 "void lightair_body_tracker_submit(in ptr tracker, in ptr data, in int linesize, in int width, "
			 "in int height, in int frame_id, out bool sent)",
			 body_submit, NULL);
	proc_handler_add(ph, "void lightair_body_tracker_get(in ptr tracker, in ptr body, out bool ok)", body_get, NULL);
}
