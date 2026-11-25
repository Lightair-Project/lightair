# Lightair Effect Stack (docs/09-effect-stack.md): the face trackers Lightair's engine calls.
# macOS: liblightair-tracker.dylib (tracking and segmentation on Core ML and Vision) and its
# compiled models in tracker-models. The presets, sticker art and shaders in data/ are built into
# the app (lightair-preview, lightair-app).
#
# The old libobs module (lightair-effect-stack.plugin on macOS, lightair-effect-stack.dll on
# Windows on ONNX Runtime with DirectML, windows.cmake) is no longer built: Lightair has no libobs.
# Its sources stay for reference; building it (LIGHTAIR_WITH_LIBOBS=ON) needs libobs's headers
# and an OBS to link, which this repository no longer has.
cmake_minimum_required(VERSION 3.24)

# Off: the trackers alone (lightair-tracker, for Lightair's engine), with nothing of libobs's or
# OBS's.
option(LIGHTAIR_WITH_LIBOBS "Build the old libobs module too" OFF)

# (WIN32 is only known after project(); the host is: Lightair builds natively on Windows)
if(CMAKE_HOST_WIN32)
  project(lightair-effect-stack VERSION 0.1.0 LANGUAGES C CXX)
  if(NOT LIGHTAIR_WITH_LIBOBS)
    message(STATUS "Lightair Effect Stack: nothing to build on Windows yet (the trackers alone are macOS's)")
    return()
  endif()
  include(cmake/windows.cmake)
  return()
endif()
project(lightair-effect-stack VERSION 0.1.0 LANGUAGES C CXX OBJC OBJCXX)

set(CMAKE_C_STANDARD 17)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_OSX_DEPLOYMENT_TARGET "14.0" CACHE STRING "")

set(ROOT "${CMAKE_SOURCE_DIR}/../..")
set(OBS_SRC "${ROOT}/third_party/obs-studio")
set(SIMDE_DIR "${ROOT}/third_party/simde")
set(OBS_APP "/Applications/OBS.app" CACHE PATH "Installed OBS.app to link against in development")
set(DEPS "${CMAKE_SOURCE_DIR}/.deps")

# Face tracking and segmentation run on Core ML: MediaPipe's face landmarker (478-point mesh),
# YuNet detection and the multiclass selfie segmenter, converted to .mlpackage
# (data/models/coreml, made by tools/convert-coreml.py) and compiled into the bundle below.

if(LIGHTAIR_WITH_LIBOBS)
set(GEN "${CMAKE_BINARY_DIR}/generated")
file(WRITE "${GEN}/obsconfig.h" "#pragma once\n#define OBS_RELEASE_CANDIDATE 0\n#define OBS_BETA 0\n")

add_library(lightair-effect-stack MODULE
  src/module-main.c
  src/face-api.c
  src/beauty-filter.c
  src/face-geometry.c
  src/makeup.c
  src/grade.c
  src/presets.c
  src/fx-layers.c
  src/shape-mask.c
  src/transform3d.c
  src/fx-filter.c
  src/tracker/tracker-core.cpp
  src/tracker/tracker-util.cpp
  src/tracker/pigment-calib.cpp
  src/tracker/mediapipe.cpp
  src/tracker/backend-coreml.mm
  src/tracker/coreml-common.mm
  src/tracker/segmenter-coreml.mm
  src/tracker/person-vision.mm
  src/virtualcam.m
  src/tracker/body-mediapipe.cpp
  src/tracker/body-tracker.cpp
  src/tracker/body-coreml.mm
)
set_source_files_properties(src/tracker/backend-coreml.mm src/tracker/coreml-common.mm src/tracker/segmenter-coreml.mm
  src/tracker/person-vision.mm src/virtualcam.m src/tracker/body-coreml.mm
  PROPERTIES COMPILE_FLAGS "-fobjc-arc")
target_compile_definitions(lightair-effect-stack PRIVATE BEAUTY_TRACKER_COREML BEAUTY_HAVE_LIBOBS PLUGIN_VERSION="${PROJECT_VERSION}")
target_include_directories(lightair-effect-stack PRIVATE "${OBS_SRC}/libobs" "${GEN}" "${SIMDE_DIR}" src src/tracker)
target_compile_options(lightair-effect-stack PRIVATE -Wall -Wno-unused-parameter -Wno-missing-field-initializers)
target_link_options(lightair-effect-stack PRIVATE "-F${OBS_APP}/Contents/Frameworks" "-Wl,-rpath,@executable_path/../Frameworks")
target_link_libraries(lightair-effect-stack PRIVATE "-framework libobs" "-framework Foundation" "-framework CoreVideo"
  "-framework CoreGraphics" "-framework Vision" "-framework CoreML" "-framework Accelerate"
  "-framework CoreMedia" "-framework CoreMediaIO")

file(GLOB_RECURSE MODULE_DATA RELATIVE "${CMAKE_SOURCE_DIR}/data" CONFIGURE_DEPENDS "${CMAKE_SOURCE_DIR}/data/*")
list(FILTER MODULE_DATA EXCLUDE REGEX "^models/")
foreach(f IN LISTS MODULE_DATA)
  get_filename_component(dir "${f}" DIRECTORY)
  target_sources(lightair-effect-stack PRIVATE "data/${f}")
  set_source_files_properties("data/${f}" PROPERTIES MACOSX_PACKAGE_LOCATION "Resources/${dir}")
endforeach()
endif()
# Core ML models, compiled to .mlmodelc and copied into Resources/models.
set(COREML_OUT "${CMAKE_BINARY_DIR}/coreml")
set(COREML_COMPILED "")
foreach(m selfie_multiclass_256 face_detection_yunet face_landmarks_478
    person_detection_mediapipe_2023mar pose_estimation_mediapipe_2023mar handpose_estimation_mediapipe_2023feb)
  set(pkg "${CMAKE_SOURCE_DIR}/data/models/coreml/${m}.mlpackage")
  if(NOT EXISTS "${pkg}")
    message(FATAL_ERROR "missing ${pkg}: run tools/convert-coreml.py")
  endif()
  file(GLOB_RECURSE pkg_files "${pkg}/*")
  add_custom_command(OUTPUT "${COREML_OUT}/${m}.mlmodelc/coremldata.bin"
    COMMAND "${CMAKE_COMMAND}" -E rm -rf "${COREML_OUT}/${m}.mlmodelc"
    COMMAND xcrun coremlcompiler compile "${pkg}" "${COREML_OUT}" > /dev/null
    DEPENDS ${pkg_files} VERBATIM)
  list(APPEND COREML_COMPILED "${COREML_OUT}/${m}.mlmodelc/coremldata.bin")
  if(LIGHTAIR_WITH_LIBOBS)
  add_custom_command(TARGET lightair-effect-stack POST_BUILD
    COMMAND "${CMAKE_COMMAND}" -E rm -rf "$<TARGET_BUNDLE_CONTENT_DIR:lightair-effect-stack>/Resources/models/${m}.mlmodelc"
    COMMAND "${CMAKE_COMMAND}" -E copy_directory "${COREML_OUT}/${m}.mlmodelc"
            "$<TARGET_BUNDLE_CONTENT_DIR:lightair-effect-stack>/Resources/models/${m}.mlmodelc")
  endif()
endforeach()
add_custom_target(coreml-models DEPENDS ${COREML_COMPILED})
if(LIGHTAIR_WITH_LIBOBS)
add_dependencies(lightair-effect-stack coreml-models)
set_target_properties(lightair-effect-stack PROPERTIES
  BUNDLE TRUE BUNDLE_EXTENSION plugin PREFIX ""
  MACOSX_BUNDLE_INFO_PLIST "${CMAKE_SOURCE_DIR}/cmake/Info.plist.in"
  MACOSX_BUNDLE_GUI_IDENTIFIER "app.lightair.effect-stack"
  MACOSX_BUNDLE_BUNDLE_VERSION "${PROJECT_VERSION}"
  MACOSX_BUNDLE_SHORT_VERSION_STRING "${PROJECT_VERSION}")
endif()

# The trackers alone (faces, segmentation, people, bodies through Core ML and Vision):
# Lightair's engine calls their C API directly (lightair-host's tracker.rs). The compiled models
# sit beside the library, in tracker-models.
add_library(lightair-tracker SHARED
  src/tracker/tracker-core.cpp
  src/tracker/tracker-util.cpp
  src/tracker/pigment-calib.cpp
  src/tracker/mediapipe.cpp
  src/tracker/backend-coreml.mm
  src/tracker/coreml-common.mm
  src/tracker/segmenter-coreml.mm
  src/tracker/person-vision.mm
  src/tracker/body-mediapipe.cpp
  src/tracker/body-tracker.cpp
  src/tracker/body-coreml.mm
)
target_compile_definitions(lightair-tracker PRIVATE BEAUTY_TRACKER_COREML)
target_include_directories(lightair-tracker PRIVATE "${SIMDE_DIR}" src src/tracker)
target_compile_options(lightair-tracker PRIVATE -Wall -Wno-unused-parameter -Wno-missing-field-initializers)
target_link_libraries(lightair-tracker PRIVATE "-framework Foundation" "-framework CoreVideo" "-framework CoreGraphics"
  "-framework Vision" "-framework CoreML" "-framework Accelerate" "-framework CoreMedia")
set_target_properties(lightair-tracker PROPERTIES INSTALL_NAME_DIR "@rpath")
add_dependencies(lightair-tracker coreml-models)
add_custom_command(TARGET lightair-tracker POST_BUILD
  COMMAND "${CMAKE_COMMAND}" -E rm -rf "$<TARGET_FILE_DIR:lightair-tracker>/tracker-models"
  COMMAND "${CMAKE_COMMAND}" -E copy_directory "${COREML_OUT}" "$<TARGET_FILE_DIR:lightair-tracker>/tracker-models")
