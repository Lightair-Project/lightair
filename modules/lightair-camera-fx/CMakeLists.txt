# Lightair Camera Studio (docs/09-camera-studio.md): a libobs module built in-tree.
# Builds lightair-camera-fx.plugin against the pinned libobs headers. Development builds link an
# installed OBS 33's libobs.framework (OBS_APP), like the rest of Lightair (IMPLEMENTATION.md).
cmake_minimum_required(VERSION 3.24)
project(lightair-camera-fx VERSION 0.1.0 LANGUAGES C CXX OBJC OBJCXX)

set(CMAKE_C_STANDARD 17)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_OSX_DEPLOYMENT_TARGET "14.0" CACHE STRING "")

set(ROOT "${CMAKE_SOURCE_DIR}/../..")
set(OBS_SRC "${ROOT}/third_party/obs-studio")
set(SIMDE_DIR "${ROOT}/third_party/simde")
set(OBS_APP "/Applications/OBS.app" CACHE PATH "Installed OBS.app to link against in development")
set(DEPS "${CMAKE_SOURCE_DIR}/.deps")
set(ORT_VERSION "1.31.1")

function(fetch url archive dest marker)
  if(NOT EXISTS "${marker}")
    message(STATUS "Downloading ${url}")
    file(DOWNLOAD "${url}" "${DEPS}/${archive}" STATUS st)
    list(GET st 0 code)
    if(NOT code EQUAL 0)
      message(FATAL_ERROR "Download failed: ${url} (${st})")
    endif()
    file(ARCHIVE_EXTRACT INPUT "${DEPS}/${archive}" DESTINATION "${dest}")
  endif()
endfunction()

# ONNX Runtime (segmentation on the Neural Engine via Core ML).
set(ORT_DIR "${DEPS}/onnxruntime-osx-arm64-${ORT_VERSION}")
fetch("https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-osx-arm64-${ORT_VERSION}.tgz"
      "ort-mac.tgz" "${DEPS}" "${ORT_DIR}/include/onnxruntime_cxx_api.h")
file(GLOB ORT_RUNTIME "${ORT_DIR}/lib/libonnxruntime.*.*.*.dylib")

# Models (not in git; licences in data/models/NOTICE.md). Vision provides landmarks on macOS,
# so only the segmenter is needed here.
set(MODEL_DIR "${CMAKE_SOURCE_DIR}/data/models")
foreach(m "selfie_multiclass_256.onnx|https://huggingface.co/senty-au/selfie_multiclass_256x256-ONNX/resolve/main/onnx/model.onnx"
          "selfie_segmentation_landscape.onnx|https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape/resolve/main/onnx/model.onnx")
  string(REPLACE "|" ";" parts "${m}")
  list(GET parts 0 name)
  list(GET parts 1 url)
  if(NOT EXISTS "${MODEL_DIR}/${name}")
    message(STATUS "Downloading model ${name}")
    file(DOWNLOAD "${url}" "${MODEL_DIR}/${name}")
  endif()
endforeach()

set(GEN "${CMAKE_BINARY_DIR}/generated")
file(WRITE "${GEN}/obsconfig.h" "#pragma once\n#define OBS_RELEASE_CANDIDATE 0\n#define OBS_BETA 0\n")

add_library(lightair-camera-fx MODULE
  src/module-main.c
  src/beauty-filter.c
  src/face-geometry.c
  src/makeup.c
  src/grade.c
  src/presets.c
  src/fx-layers.c
  src/shape-mask.c
  src/tracker/tracker-core.cpp
  src/tracker/onnx-common.cpp
  src/tracker/segmenter.cpp
  src/tracker/pigment-calib.cpp
  src/tracker/backend-vision.mm
)
set_source_files_properties(src/tracker/backend-vision.mm PROPERTIES COMPILE_FLAGS "-fobjc-arc")
target_compile_definitions(lightair-camera-fx PRIVATE BEAUTY_TRACKER_VISION BEAUTY_HAVE_LIBOBS PLUGIN_VERSION="${PROJECT_VERSION}")
target_include_directories(lightair-camera-fx PRIVATE "${OBS_SRC}/libobs" "${GEN}" "${SIMDE_DIR}" "${ORT_DIR}/include" src src/tracker)
target_compile_options(lightair-camera-fx PRIVATE -Wall -Wno-unused-parameter -Wno-missing-field-initializers)
target_link_options(lightair-camera-fx PRIVATE "-F${OBS_APP}/Contents/Frameworks" "-Wl,-rpath,@executable_path/../Frameworks")
target_link_libraries(lightair-camera-fx PRIVATE "-framework libobs" "-framework Foundation" "-framework CoreVideo"
  "-framework CoreGraphics" "-framework Vision" "-framework CoreML")

file(GLOB_RECURSE MODULE_DATA RELATIVE "${CMAKE_SOURCE_DIR}/data" CONFIGURE_DEPENDS "${CMAKE_SOURCE_DIR}/data/*")
list(FILTER MODULE_DATA EXCLUDE REGEX "^models/face_")
foreach(f IN LISTS MODULE_DATA)
  get_filename_component(dir "${f}" DIRECTORY)
  target_sources(lightair-camera-fx PRIVATE "data/${f}")
  set_source_files_properties("data/${f}" PROPERTIES MACOSX_PACKAGE_LOCATION "Resources/${dir}")
endforeach()
target_sources(lightair-camera-fx PRIVATE "${ORT_RUNTIME}")
set_source_files_properties("${ORT_RUNTIME}" PROPERTIES MACOSX_PACKAGE_LOCATION "Frameworks")
get_filename_component(ORT_NAME "${ORT_RUNTIME}" NAME)
target_compile_definitions(lightair-camera-fx PRIVATE BEAUTY_ORT_FILE="${ORT_NAME}")
set_target_properties(lightair-camera-fx PROPERTIES
  BUNDLE TRUE BUNDLE_EXTENSION plugin PREFIX ""
  MACOSX_BUNDLE_INFO_PLIST "${CMAKE_SOURCE_DIR}/cmake/Info.plist.in"
  MACOSX_BUNDLE_GUI_IDENTIFIER "app.lightair.camera-fx"
  MACOSX_BUNDLE_BUNDLE_VERSION "${PROJECT_VERSION}"
  MACOSX_BUNDLE_SHORT_VERSION_STRING "${PROJECT_VERSION}")
