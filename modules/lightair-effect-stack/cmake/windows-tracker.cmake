# The trackers alone on Windows (lightair-tracker.dll, for Lightair's engine: lightair-host's
# tracker.rs): faces, segmentation, people and bodies on ONNX Runtime with the DirectML execution
# provider (any DirectX 12 GPU; CPU as a fallback), through the module's ONNX backends.
#
# Output, beside each other (what tracker.rs looks for):
#   <build>/lightair-tracker.dll, onnxruntime.dll, DirectML.dll
#   <build>/tracker-models/*.onnx
#
# Everything fetched is pinned and checked: Microsoft's NuGet packages by version, the models
# (data/models/NOTICE.md) by their repositories' commits and SHA-256. They land in .deps (git
# ignores it), fetched once.

set(CMAKE_C_STANDARD 17)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(ROOT "${CMAKE_SOURCE_DIR}/../..")
set(SIMDE_DIR "${ROOT}/third_party/simde")
set(DEPS "${CMAKE_SOURCE_DIR}/.deps")

# Microsoft's NuGet packages: headers, onnxruntime.dll, DirectML.dll.
set(ORT_VERSION 1.20.1)
set(DML_VERSION 1.15.4)
function(nuget name version dir)
  if(NOT EXISTS "${dir}/.done")
    set(pkg "${DEPS}/${name}.${version}.nupkg")
    file(DOWNLOAD "https://www.nuget.org/api/v2/package/${name}/${version}" "${pkg}" STATUS st TLS_VERIFY ON)
    list(GET st 0 code)
    if(NOT code EQUAL 0)
      message(FATAL_ERROR "could not download ${name} ${version}: ${st}")
    endif()
    file(ARCHIVE_EXTRACT INPUT "${pkg}" DESTINATION "${dir}")
    file(REMOVE "${pkg}")
    file(TOUCH "${dir}/.done")
  endif()
endfunction()
nuget(Microsoft.ML.OnnxRuntime.DirectML ${ORT_VERSION} "${DEPS}/ort")
nuget(Microsoft.AI.DirectML ${DML_VERSION} "${DEPS}/dml")

# The models, by the names the backends load them by: repository, commit, file, SHA-256.
set(MODELS_DIR "${DEPS}/models")
function(model name repo commit file sha256)
  set(out "${MODELS_DIR}/${name}")
  if(EXISTS "${out}")
    file(SHA256 "${out}" have)
    if(have STREQUAL sha256)
      return()
    endif()
  endif()
  message(STATUS "Lightair Effect Stack: fetching ${name}")
  file(DOWNLOAD "https://huggingface.co/${repo}/resolve/${commit}/${file}" "${out}"
    EXPECTED_HASH SHA256=${sha256} STATUS st TLS_VERIFY ON)
  list(GET st 0 code)
  if(NOT code EQUAL 0)
    file(REMOVE "${out}")
    message(FATAL_ERROR "could not download ${name}: ${st}")
  endif()
endfunction()
model(face_detection_yunet.onnx opencv/face_detection_yunet 3cc26e7f1014a5ee5d74a42acee58bafc9d0a310
  face_detection_yunet_2023mar.onnx 8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4)
model(face_landmarks_478.onnx senty-au/face_landmarks_detector-ONNX 337d58218b5b1cc597ca3c67360880b920f6ce7b
  onnx/model.onnx 7d6e82dee82a1dca5fbddb282b3cc74571833a530de317fc22ae325c3358beeb)
model(selfie_multiclass_256.onnx senty-au/selfie_multiclass_256x256-ONNX 6db8421a7150ac20558f2c24675078eb3a1a04d0
  onnx/model.onnx 35ec1ecd9ee7f85073c99c00020b7f6751b69506eeacf683bc8665f6117f85b0)
model(selfie_segmentation_landscape.onnx onnx-community/mediapipe_selfie_segmentation_landscape
  2497d5bec26c626c7b3c4edc6e1fefc21b64f6c3 onnx/model.onnx
  7a0adcfdb1715d3b0ff0f61486d8a181c33f4a208343a5abfccbc6540e872d24)
model(person_detection_mediapipe_2023mar.onnx opencv/person_detection_mediapipe 531b6da87204aa40aa5cc680b4d032dd09b5055e
  person_detection_mediapipe_2023mar.onnx 47fd5599d6fa17608f03e0eb0ae230baa6e597d7e8a2c8199fe00abea55a701f)
model(pose_estimation_mediapipe_2023mar.onnx opencv/pose_estimation_mediapipe 8cd55743af63a8578effaf144a68cb58f6450fe9
  pose_estimation_mediapipe_2023mar.onnx 9d89c599319a18fb7d2e28451a883476164543182bafca5f09eb2cf767ed2f3f)
model(handpose_estimation_mediapipe_2023feb.onnx opencv/handpose_estimation_mediapipe
  4b2a0b446e5cf2f11fb6b2c7251091c035d2c1f7 handpose_estimation_mediapipe_2023feb.onnx
  db0898ae717b76b075d9bf563af315b29562e11f8df5027a1ef07b02bef6d81c)

add_library(lightair-tracker SHARED
  src/tracker/tracker-core.cpp
  src/tracker/tracker-util.cpp
  src/tracker/pigment-calib.cpp
  src/tracker/mediapipe.cpp
  src/tracker/backend-onnx.cpp
  src/tracker/onnx-common.cpp
  src/tracker/segmenter.cpp
  src/tracker/body-mediapipe.cpp
  src/tracker/body-tracker.cpp
  src/tracker/body-onnx.cpp
)
target_compile_definitions(lightair-tracker PRIVATE _USE_MATH_DEFINES NOMINMAX _CRT_SECURE_NO_WARNINGS)
target_include_directories(lightair-tracker PRIVATE "${SIMDE_DIR}" src src/tracker "${DEPS}/ort/build/native/include")
target_link_libraries(lightair-tracker PRIVATE dxgi)
# the C API (face-tracker.h, body-tracker.h) exported as it's declared
set_target_properties(lightair-tracker PROPERTIES WINDOWS_EXPORT_ALL_SYMBOLS ON)
# one folder whatever the configuration (Visual Studio's generator adds one per configuration)
foreach(config "" _DEBUG _RELEASE _RELWITHDEBINFO _MINSIZEREL)
  set_target_properties(lightair-tracker PROPERTIES RUNTIME_OUTPUT_DIRECTORY${config} "${CMAKE_BINARY_DIR}"
    LIBRARY_OUTPUT_DIRECTORY${config} "${CMAKE_BINARY_DIR}" ARCHIVE_OUTPUT_DIRECTORY${config} "${CMAKE_BINARY_DIR}")
endforeach()
if(MSVC)
  target_compile_options(lightair-tracker PRIVATE /W3 /utf-8)
endif()

# the runtime and the models beside the library
add_custom_command(TARGET lightair-tracker POST_BUILD
  COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${DEPS}/ort/runtimes/win-x64/native/onnxruntime.dll" "${CMAKE_BINARY_DIR}/"
  COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${DEPS}/dml/bin/x64-win/DirectML.dll" "${CMAKE_BINARY_DIR}/"
  COMMAND "${CMAKE_COMMAND}" -E copy_directory "${MODELS_DIR}" "${CMAKE_BINARY_DIR}/tracker-models"
  VERBATIM)
