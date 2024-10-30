use mls_ops::{decrypt_msg, encrypt_msg, WelcomePackageOut, WorkerResponse};
use openmls::prelude::tls_codec::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    console,
    js_sys::{
        Array, ArrayBuffer, JsString, Object,
        Reflect::{get as obj_get, set as obj_set},
        Uint8Array,
    },
    ReadableStream, ReadableStreamDefaultReader, RtcEncodedAudioFrame, RtcEncodedVideoFrame,
    WritableStream, WritableStreamDefaultWriter,
};

mod mls_ops;

/// Given an `RtcEncodedAudioFrame` or `RtcEncodedVideoFrame`, returns the frame's byte contents
fn get_frame_data(frame: &JsValue) -> Vec<u8> {
    if RtcEncodedAudioFrame::instanceof(frame) {
        let frame: &RtcEncodedAudioFrame = frame.dyn_ref().unwrap();
        Uint8Array::new(&frame.data()).to_vec()
    } else if RtcEncodedVideoFrame::instanceof(frame) {
        let frame: &RtcEncodedVideoFrame = frame.dyn_ref().unwrap();
        Uint8Array::new(&frame.data()).to_vec()
    } else {
        panic!("frame value of unknown type");
    }
}

/// Given an `RtcEncodedAudioFrame` or `RtcEncodedVideoFrame` and a bytestring, sets frame's bytestring
fn set_frame_data(frame: &JsValue, new_data: &[u8]) {
    // Copy the new data into an ArrayBuffer
    let buf = ArrayBuffer::new(new_data.len() as u32);
    let view = Uint8Array::new(&buf);
    view.copy_from(new_data);

    if RtcEncodedAudioFrame::instanceof(frame) {
        let frame: &RtcEncodedAudioFrame = frame.dyn_ref().unwrap();
        frame.set_data(&buf);
    } else if RtcEncodedVideoFrame::instanceof(frame) {
        let frame: &RtcEncodedVideoFrame = frame.dyn_ref().unwrap();
        frame.set_data(&buf);
    } else {
        panic!("frame value of unknown type");
    }
}

/// Processes an event and returns an object that's null, i.e., no return value, or consists of
/// fields "type": str, "payload_name": str, and "payload": ArrayBuffer.
#[wasm_bindgen]
#[allow(non_snake_case)]
pub async fn processEvent(event: Object) -> JsValue {
    let ty = obj_get(&event, &"type".into())
        .unwrap()
        .as_string()
        .unwrap();
    let ty = ty.as_str();
    console::log_1(&format!("Received event of type {ty} from main thread").into());

    let ret = match ty {
        "encryptStream" | "decryptStream" => {
            // Grab the streams from the object and pass them to `process_stream`
            let read_stream: ReadableStream =
                obj_get(&event, &"in".into()).unwrap().dyn_into().unwrap();
            let write_stream: WritableStream =
                obj_get(&event, &"out".into()).unwrap().dyn_into().unwrap();
            let reader = ReadableStreamDefaultReader::new(&read_stream).unwrap();
            let writer = write_stream.get_writer().unwrap();

            if ty == "encryptStream" {
                process_stream(reader, writer, encrypt_msg).await;
            } else {
                process_stream(reader, writer, decrypt_msg).await;
            }
            None
        }

        "initialize" => {
            let user_id = obj_get(&event, &"id".into()).unwrap().as_string().unwrap();
            Some(mls_ops::new_state(&user_id))
        }

        "initializeAndCreateGroup" => {
            let user_id = obj_get(&event, &"id".into()).unwrap().as_string().unwrap();
            Some(mls_ops::new_state_and_start_group(&user_id))
        }

        "userJoined" => {
            let key_pkg_bytes: ArrayBuffer = obj_get(&event, &"keyPkg".into())
                .unwrap()
                .dyn_into()
                .unwrap();
            let key_pkg_bytes = Uint8Array::new(&key_pkg_bytes).to_vec();
            Some(mls_ops::add_user(&key_pkg_bytes))
        }

        _ => panic!("unknown message type {ty} from main thread"),
    };

    // Now we have to format our response. We're gonna make a list of objects to send to the main
    // thread, and a list of the buffers in each object (we need these in order to properly transfer
    // data between threads)
    let obj_list = Array::new();
    let buffers_list = Array::new();
    if let Some(WorkerResponse {
        welcome,
        proposals,
        new_safety_number,
        key_pkg,
    }) = ret
    {
        // Make the safety number object if a new safety number is given
        if let Some(sn) = new_safety_number {
            let (o, buffers) = make_obj_and_save_buffers("newSafetyNumber", &[("hash", &sn)]);

            // Accumulate the object and buffers
            obj_list.push(&o);
            buffers_list.push(&buffers);
        }

        // Make the key package object if a key package is given
        if let Some(kp) = key_pkg {
            let (o, buffers) = make_obj_and_save_buffers(
                "shareKeyPackage",
                &[("keyPkg", &kp.tls_serialize_detached().unwrap())],
            );

            // Accumulate the object and buffers
            obj_list.push(&o);
            buffers_list.push(&buffers);
        }

        // Make the welcome object if a welcome package is given
        if let Some(WelcomePackageOut {
            welcome,
            ratchet_tree,
        }) = welcome
        {
            let (o, buffers) = make_obj_and_save_buffers(
                "sendMlsWelcome",
                &[
                    ("welcome", &welcome.to_bytes().unwrap()),
                    ("rtree", &ratchet_tree.tls_serialize_detached().unwrap()),
                ],
            );

            // Accumulate the object and buffers
            obj_list.push(&o);
            buffers_list.push(&buffers);
        }

        // Make MLS message objects if messages are given
        for msg in proposals {
            let (o, buffers) = make_obj_and_save_buffers(
                "sendMlsMessage",
                &[("msg", &msg.tls_serialize_detached().unwrap())],
            );

            // Accumulate the object and buffers
            obj_list.push(&o);
            buffers_list.push(&buffers);
        }
    }

    // Finally, return an array [objs, payloads] for the worker JS script to go through and post to
    // the calling thread
    let ret = Array::new();
    ret.push(&obj_list);
    ret.push(&buffers_list);
    ret.into()
}

/// Processes a posssibly infinite stream of `RtcEncodedAudio(/Video)Frame`s . Reads a frame from
/// `reader`, applies `f` to the frame data, then writes the output to `writer`.
async fn process_stream<F>(
    reader: ReadableStreamDefaultReader,
    writer: WritableStreamDefaultWriter,
    f: F,
) where
    F: Fn(&[u8]) -> Vec<u8>,
{
    loop {
        let promise = reader.read();

        // Await the call. This will return an object { value, done }, where
        // value is a view containing the new data, and done is a bool indicating
        // that there is nothing left to read
        let res: Object = JsFuture::from(promise).await.unwrap().dyn_into().unwrap();
        let done_reading = obj_get(&res, &"done".into()).unwrap().as_bool().unwrap();

        // Read a frame and get the underlying bytestring
        let frame = obj_get(&res, &"value".into()).unwrap();

        // Process the frame data
        let frame_data = get_frame_data(&frame);
        let chunk_len = frame_data.len();
        console::log_1(&format!("Read chunk of size {chunk_len}").into());
        let new_frame_data = f(&frame_data);

        // Set the new frame data value
        set_frame_data(&frame, &new_frame_data);

        // Write the read chunk to the writable stream. This promise returns nothing
        let promise = writer.write_with_chunk(&frame);
        JsFuture::from(promise).await.unwrap();

        if done_reading {
            break;
        }
    }
}

/// Helper function. Given an object name and named bytestrings, returns the object
/// `{ type: name, [b[0]: b[1] as ArrayBuffer for b in bytestrings] },`
/// as well as the list
/// `[b[1] as ArrayBuffer for b in bytestrings]`
fn make_obj_and_save_buffers(name: &str, named_bytestrings: &[(&str, &[u8])]) -> (Object, Array) {
    let o = Object::new();
    let buffers = Array::new();
    // Make the object { type: name, ...}
    obj_set(&o, &"type".into(), &name.into()).unwrap();

    // Make the bytestrings into JS ArrayBuffers and add them to the object and buffer list
    for (field_name, bytes) in named_bytestrings {
        let arr = {
            let buf = ArrayBuffer::new(bytes.len() as u32);
            Uint8Array::new(&buf).copy_from(&bytes);
            buf
        };

        obj_set(&o, &(*field_name).into(), &arr).unwrap();
        buffers.push(&arr);
    }

    (o, buffers)
}
