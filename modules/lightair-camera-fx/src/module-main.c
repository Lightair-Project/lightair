/*
Lightair Camera Studio: face-aware camera pipeline (docs/09-camera-studio.md).
Built from the Dewy beauty pipeline. Copyright (C) 2026 Bregas S. Wicaksono.

This program is free software; you can redistribute it and/or modify it under the terms of the
GNU General Public License as published by the Free Software Foundation; either version 3 of
the License, or (at your option) any later version.
*/

#include <obs-module.h>
#include <util/dstr.h>
#include <string.h>

#include "makeup.h"
#include "presets.h"
#include "face-tracker.h"

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("lightair-camera-fx", "en-US")

extern struct obs_source_info beauty_filter_info;

const char *obs_module_name(void)
{
	return "Lightair Camera Studio";
}

const char *obs_module_description(void)
{
	return "Face-aware skin, shape, makeup, effects, atmosphere, colour and light for cameras.";
}

/* ONNX Runtime ships inside the plugin bundle (Contents/Frameworks), loaded by full path so it
 * can't clash with another copy in the process. */
static void set_runtime_path(void)
{
#ifdef BEAUTY_ORT_FILE
	const char *bin = obs_get_module_binary_path(obs_current_module());
	if (!bin)
		return;
	struct dstr p = {0};
	dstr_copy(&p, bin);
	char *slash = strrchr(p.array, '/');
	if (slash)
		*slash = 0;
	p.len = strlen(p.array);
	dstr_cat(&p, "/../Frameworks/" BEAUTY_ORT_FILE);
	face_tracker_set_runtime_library(p.array);
	dstr_free(&p);
#endif
}

bool obs_module_load(void)
{
	set_runtime_path();
	makeup_presets_load();
	presets_load();
	obs_register_source(&beauty_filter_info);
	blog(LOG_INFO, "[camera-studio] loaded");
	return true;
}

void obs_module_unload(void)
{
	makeup_presets_free();
	presets_free();
}
