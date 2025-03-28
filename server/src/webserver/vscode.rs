/// Copyright (C) 2023 Bryan A. Jones.
///
/// This file is part of the CodeChat Editor. The CodeChat Editor is free
/// software: you can redistribute it and/or modify it under the terms of the
/// GNU General Public License as published by the Free Software Foundation,
/// either version 3 of the License, or (at your option) any later version.
///
/// The CodeChat Editor is distributed in the hope that it will be useful, but
/// WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
/// or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
/// more details.
///
/// You should have received a copy of the GNU General Public License along with
/// the CodeChat Editor. If not, see
/// [http://www.gnu.org/licenses](http://www.gnu.org/licenses).
///
/// `vscode.rs` -- Implement server-side functionality for the Visual Studio
/// Code IDE
/// ========================================================================
// Imports
// -------
//
// ### Standard library
use std::{cmp::min, collections::HashMap, path::PathBuf};

// ### Third-party
use actix_web::{
    HttpRequest, HttpResponse,
    error::{Error, ErrorBadRequest},
    get, web,
};
use indoc::formatdoc;
use log::{debug, error, warn};
use open;
use tokio::{select, sync::mpsc};

// ### Local
use super::{
    AppState, EditorMessage, EditorMessageContents, IP_ADDRESS, IdeType, WebsocketQueues,
    client_websocket, get_client_framework, send_response,
};
use crate::{
    oneshot_send,
    processing::{
        CodeChatForWeb, CodeMirror, SourceFileMetadata, TranslationResultsString,
        codechat_for_web_to_source, source_to_codechat_for_web_string,
    },
    queue_send,
    webserver::{
        INITIAL_MESSAGE_ID, MESSAGE_ID_INCREMENT, ProcessingTaskHttpRequest, ResultOkTypes,
        UpdateMessageContents, escape_html, filesystem_endpoint, html_wrapper,
        make_simple_http_response, path_to_url, text_file_to_response, try_canonicalize,
        url_to_path,
    },
};

// Globals
// -------
const VSCODE_PATH_PREFIX: &[&str] = &["vsc", "fs"];
// The max length of a message to show in the console.
const MAX_MESSAGE_LENGTH: usize = 200;

// Code
// ----
//
// This is the processing task for the Visual Studio Code IDE. It handles all
// the core logic to moving data between the IDE and the client.
#[get("/vsc/ws-ide/{connection_id}")]
pub async fn vscode_ide_websocket(
    connection_id: web::Path<String>,
    req: HttpRequest,
    body: web::Payload,
    app_state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let connection_id_str = connection_id.to_string();

    // There are three cases for this `connection_id`:
    //
    // 1.  It hasn't been used before. In this case, create the appropriate
    //     queues and start websocket and processing tasks.
    // 2.  It's in use, but was disconnected. In this case, re-use the queues
    //     and start the websocket task; the processing task is still running.
    // 3.  It's in use by another IDE. This is an error, but I don't have a way
    //     to detect it yet.
    //
    // Check case 3.
    if app_state
        .vscode_connection_id
        .lock()
        .unwrap()
        .contains(&connection_id_str)
    {
        let msg = format!("Connection ID {connection_id_str} already in use.");
        error!("{msg}");
        return Err(ErrorBadRequest(msg));
    }

    // Now case 2.
    if app_state
        .vscode_ide_queues
        .lock()
        .unwrap()
        .contains_key(&connection_id_str)
    {
        return client_websocket(
            connection_id,
            req,
            body,
            app_state.vscode_ide_queues.clone(),
        )
        .await;
    }

    // Then this is case 1. Add the connection ID to the list of active
    // connections.
    let (from_ide_tx, mut from_ide_rx) = mpsc::channel(10);
    let (to_ide_tx, to_ide_rx) = mpsc::channel(10);
    assert!(
        app_state
            .vscode_ide_queues
            .lock()
            .unwrap()
            .insert(
                connection_id_str.clone(),
                WebsocketQueues {
                    from_websocket_tx: from_ide_tx,
                    to_websocket_rx: to_ide_rx,
                },
            )
            .is_none()
    );
    let (from_client_tx, mut from_client_rx) = mpsc::channel(10);
    let (to_client_tx, to_client_rx) = mpsc::channel(10);
    assert!(
        app_state
            .vscode_client_queues
            .lock()
            .unwrap()
            .insert(
                connection_id_str.clone(),
                WebsocketQueues {
                    from_websocket_tx: from_client_tx,
                    to_websocket_rx: to_client_rx,
                },
            )
            .is_none()
    );
    app_state
        .vscode_connection_id
        .lock()
        .unwrap()
        .insert(connection_id_str.clone());

    // Clone variables owned by the processing task.
    let connection_id_task = connection_id_str.clone();
    let app_state_task = app_state.clone();

    // Start the processing task.
    actix_rt::spawn(async move {
        // Use a [labeled block
        // expression](https://doc.rust-lang.org/reference/expressions/loop-expr.html#labelled-block-expressions)
        // to provide a way to exit the current task.
        'task: {
            let mut current_file = PathBuf::new();
            let mut load_file_requests: HashMap<u64, ProcessingTaskHttpRequest> = HashMap::new();
            debug!("VSCode processing task started.");

            // Get the first message sent by the IDE.
            let Some(first_message): std::option::Option<EditorMessage> = from_ide_rx.recv().await
            else {
                error!("{}", "IDE websocket received no data.");
                break 'task;
            };

            // Make sure it's the `Opened` message.
            let EditorMessageContents::Opened(ide_type) = first_message.message else {
                let msg = format!("Unexpected message {first_message:?}");
                error!("{msg}");
                send_response(&to_ide_tx, first_message.id, Err(msg)).await;

                // Send a `Closed` message to shut down the websocket.
                queue_send!(to_ide_tx.send(EditorMessage { id: 0.0, message: EditorMessageContents::Closed}), 'task);
                break 'task;
            };
            debug!("Received IDE Opened message.");

            // Ensure the IDE type (VSCode) is correct.
            match ide_type {
                IdeType::VSCode(is_self_hosted) => {
                    if is_self_hosted {
                        // Send a response (successful) to the `Opened` message.
                        debug!(
                            "Sending response = OK to IDE Opened message, id {}.",
                            first_message.id
                        );
                        send_response(&to_ide_tx, first_message.id, Ok(ResultOkTypes::Void)).await;

                        // Send the HTML for the internal browser.
                        let port = app_state_task.port;
                        let client_html = formatdoc!(
                            r#"
                            <!DOCTYPE html>
                            <html>
                                <head>
                                </head>
                                <body style="margin: 0px; padding: 0px; overflow: hidden">
                                    <iframe src="http://{IP_ADDRESS}:{port}/vsc/cf/{connection_id_task}" style="width: 100%; height: 100vh; border: none"></iframe>
                                </body>
                            </html>"#
                        );
                        debug!("Sending ClientHtml message to IDE: {client_html}");
                        queue_send!(to_ide_tx.send(EditorMessage {
                            id: 0.0,
                            message: EditorMessageContents::ClientHtml(client_html)
                        }), 'task);

                        // Wait for the response.
                        let Some(message) = from_ide_rx.recv().await else {
                            error!("{}", "IDE websocket received no data.");
                            break 'task;
                        };

                        // Make sure it's the `Result` message with no errors.
                        let res =
                            // First, make sure the ID matches.
                            if message.id != 0.0 {
                                Err(format!("Unexpected message ID {}.", message.id))
                            } else {
                                match message.message {
                                    EditorMessageContents::Result(message_result) => match message_result {
                                        Err(err) => Err(format!("Error in ClientHtml: {err}")),
                                        Ok(result_ok) =>
                                            if let ResultOkTypes::Void = result_ok {
                                                Ok(())
                                            } else {
                                                Err(format!(
                                                    "Unexpected message LoadFile contents {result_ok:?}."
                                                ))
                                            }
                                    },
                                    _ => Err(format!("Unexpected message {message:?}")),
                                }
                            };
                        if let Err(err) = res {
                            error!("{err}");
                            // Send a `Closed` message.
                            queue_send!(to_ide_tx.send(EditorMessage {
                                id: 1.0,
                                message: EditorMessageContents::Closed
                            }), 'task);
                            break 'task;
                        };
                    } else {
                        // Open the Client in an external browser.
                        if let Err(err) = open::that_detached(format!(
                            "http://{IP_ADDRESS}:{}/vsc/cf/{connection_id_task}",
                            app_state_task.port
                        )) {
                            let msg = format!("Unable to open web browser: {err}");
                            error!("{msg}");
                            send_response(&to_ide_tx, first_message.id, Err(msg)).await;

                            // Send a `Closed` message.
                            queue_send!(to_ide_tx.send(EditorMessage{
                                id: 0.0,
                                message: EditorMessageContents::Closed
                            }), 'task);
                            break 'task;
                        }
                        // Send a response (successful) to the `Opened` message.
                        send_response(&to_ide_tx, first_message.id, Ok(ResultOkTypes::Void)).await;
                    }
                }
                _ => {
                    // This is the wrong IDE type. Report then error.
                    let msg = format!("Invalid IDE type: {ide_type:?}");
                    error!("{msg}");
                    send_response(&to_ide_tx, first_message.id, Err(msg)).await;

                    // Close the connection.
                    queue_send!(to_ide_tx.send(EditorMessage { id: 0.0, message: EditorMessageContents::Closed}), 'task);
                    break 'task;
                }
            }

            // Create a queue for HTTP requests fo communicate with this task.
            let (from_http_tx, mut from_http_rx) = mpsc::channel(10);
            app_state_task
                .processing_task_queue_tx
                .lock()
                .unwrap()
                .insert(connection_id_task.to_string(), from_http_tx);

            // All further messages are handled in the main loop.
            let mut id: f64 = INITIAL_MESSAGE_ID + MESSAGE_ID_INCREMENT;
            loop {
                select! {
                    // Look for messages from the IDE.
                    Some(ide_message) = from_ide_rx.recv() => {
                        let msg = format!("{:?}", ide_message.message);
                        debug!("Received IDE message id = {}, message = {}", ide_message.id, &msg[..min(MAX_MESSAGE_LENGTH, msg.len())]);
                        match ide_message.message {
                            // Handle messages that the IDE must not send.
                            EditorMessageContents::Opened(_) |
                            EditorMessageContents::OpenUrl(_) |
                            EditorMessageContents::LoadFile(_) |
                            EditorMessageContents::ClientHtml(_) => {
                                let msg = "IDE must not send this message.";
                                error!("{msg}");
                                send_response(&to_ide_tx, ide_message.id, Err(msg.to_string())).await;
                            },

                            // Handle messages that are simply passed through.
                            EditorMessageContents::Closed |
                            EditorMessageContents::RequestClose => {
                                debug!("Forwarding it to the Client.");
                                queue_send!(to_client_tx.send(ide_message))
                            },

                            // Pass a `Result` message to the Client, unless
                            // it's a `LoadFile` result.
                            EditorMessageContents::Result(ref result) => {
                                let is_loadfile = match result {
                                    // See if this error was produced by a
                                    // `LoadFile` result.
                                    Err(_) => load_file_requests.contains_key(&ide_message.id.to_bits()),
                                    Ok(result_ok) => match result_ok {
                                        ResultOkTypes::Void => false,
                                        ResultOkTypes::LoadFile(_) => true,
                                    }
                                };
                                // Pass the message to the client if this isn't
                                // a `LoadFile` result (the only type of result
                                // which the Server should handle).
                                if !is_loadfile {
                                    debug!("Forwarding it to the Client.");
                                    queue_send!(to_client_tx.send(ide_message));
                                    continue;
                                }
                                // Ensure there's an HTTP request for this
                                // `LoadFile` result.
                                let Some(http_request) = load_file_requests.remove(&ide_message.id.to_bits()) else {
                                    error!("Error: no HTTP request found for LoadFile result ID {}.", ide_message.id);
                                    break 'task;
                                };

                                // Get the file contents from a `LoadFile`
                                // result; otherwise, this is None.
                                let file_contents_option = match result {
                                    Err(err) => {
                                        error!("{err}");
                                        &None
                                    },
                                    Ok(result_ok) => match result_ok {
                                        ResultOkTypes::Void => panic!("LoadFile result should not be void."),
                                        ResultOkTypes::LoadFile(file_contents) => file_contents,
                                    }
                                };

                                // Process the file contents.
                                let (simple_http_response, option_update) = match file_contents_option {
                                    Some(file_contents) =>
                                        text_file_to_response(&http_request, &current_file, &http_request.file_path, file_contents).await,
                                    None => {
                                        // The file wasn't available in the IDE.
                                        // Look for it in the filesystem.
                                        debug!("Sending HTTP response.");
                                        make_simple_http_response(&http_request, &current_file).await
                                    }
                                };
                                if let Some(update) = option_update {
                                    // Send the update to the client.
                                    debug!("Sending Update to Client, id = {id}.");
                                    queue_send!(to_client_tx.send(EditorMessage { id, message: update }));
                                    id += MESSAGE_ID_INCREMENT;
                                }
                                oneshot_send!(http_request.response_queue.send(simple_http_response));
                            }

                            // Handle the `Update` message.
                            EditorMessageContents::Update(update) => {
                                // Normalize the provided file name.
                                let result = match try_canonicalize(&update.file_path) {
                                    Err(err) => Err(err),
                                    Ok(clean_file_path) => {
                                        match &update.contents {
                                            None => Err("TODO: support for updates without contents.".to_string()),
                                            Some(contents) => {
                                                // Translate the file.
                                                let (translation_results_string, _path_to_toc) =
                                                source_to_codechat_for_web_string(&contents.source.doc, &current_file, false);
                                                match translation_results_string {
                                                    TranslationResultsString::CodeChat(cc) => {
                                                        // Send the new translated contents.
                                                        debug!("Sending translated contents to Client.");
                                                        queue_send!(to_client_tx.send(EditorMessage {
                                                            id: ide_message.id,
                                                            message: EditorMessageContents::Update(UpdateMessageContents {
                                                                file_path: clean_file_path.to_str().expect("Since the path started as a string, assume it losslessly translates back to a string.").to_string(),
                                                                contents: Some(cc),
                                                                cursor_position: None,
                                                                scroll_position: None,
                                                            }),
                                                        }));
                                                        Ok(ResultOkTypes::Void)
                                                    }
                                                    TranslationResultsString::Err(err) => Err(format!("Error translating source to CodeChat: {err}").to_string()),
                                                    TranslationResultsString::Unknown => {
                                                        // Send the new raw contents.
                                                        debug!("Sending translated contents to Client.");
                                                        queue_send!(to_client_tx.send(EditorMessage {
                                                            id: ide_message.id,
                                                            message: EditorMessageContents::Update(UpdateMessageContents {
                                                                file_path: clean_file_path.to_str().expect("Since the path started as a string, assume it losslessly translates back to a string.").to_string(),
                                                                contents: Some(CodeChatForWeb {
                                                                    metadata: SourceFileMetadata {
                                                                        // Since this is raw data, `mode` doesn't
                                                                        // matter.
                                                                        mode: "".to_string()
                                                                    },
                                                                    source: CodeMirror {
                                                                        doc: contents.source.doc.clone(),
                                                                        doc_blocks: vec![]
                                                                    }
                                                                }),
                                                                cursor_position: None,
                                                                scroll_position: None,
                                                            }),
                                                        }));
                                                        Ok(ResultOkTypes::Void)
                                                    },
                                                    TranslationResultsString::Toc(_) => {
                                                        Err("Error: source incorrectly recognized as a TOC.".to_string())
                                                    }
                                                }
                                            }
                                        }
                                    }
                                };
                                // If there's an error, then report it;
                                // otherwise, the message is passed to the
                                // Client, which will provide the result.
                                if let Err(err) = &result {
                                    error!("{err}");
                                    send_response(&to_ide_tx, ide_message.id, result).await;
                                }
                            }

                            // Update the current file; translate it to a URL
                            // then pass it to the Client.
                            EditorMessageContents::CurrentFile(file_path) => {
                                debug!("Translating and forwarding it to the Client.");
                                match try_canonicalize(&file_path) {
                                    Ok(clean_file_path) => {
                                        queue_send!(to_client_tx.send(EditorMessage {
                                            id: ide_message.id,
                                            message: EditorMessageContents::CurrentFile(
                                                path_to_url("/vsc/fs", &connection_id_task, &clean_file_path)
                                            )
                                        }));
                                        current_file = file_path.into();
                                    }
                                    Err(err) => {
                                        let msg = format!(
                                            "Unable to canonicalize file name {}: {err}", &file_path
                                        );
                                        error!("{msg}");
                                        send_response(&to_client_tx, ide_message.id, Err(msg)).await;
                                    }
                                }
                            }
                        }
                    },

                    // Handle HTTP requests.
                    Some(http_request) = from_http_rx.recv() => {
                        debug!("Received HTTP request for {:?} and sending LoadFile to IDE, id = {id}.", http_request.file_path);
                        // Convert the request into a `LoadFile` message.
                        queue_send!(to_ide_tx.send(EditorMessage {
                            id,
                            message: EditorMessageContents::LoadFile(http_request.file_path.clone())
                        }));
                        // Store the ID and request, which are needed to send a
                        // response when the `LoadFile` result is received.
                        load_file_requests.insert(id.to_bits(), http_request);
                        id += MESSAGE_ID_INCREMENT;
                    }

                    // Handle messages from the client.
                    Some(client_message) = from_client_rx.recv() => {
                        let msg = format!("{:?}", client_message.message);
                        debug!("Received Client message id = {}, message = {}", client_message.id, &msg[..min(MAX_MESSAGE_LENGTH, msg.len())]);
                        match client_message.message {
                            // Handle messages that the client must not send.
                            EditorMessageContents::Opened(_) |
                            EditorMessageContents::LoadFile(_) |
                            EditorMessageContents::RequestClose |
                            EditorMessageContents::ClientHtml(_) => {
                                let msg = "Client must not send this message.";
                                error!("{msg}");
                                send_response(&to_client_tx, client_message.id, Err(msg.to_string())).await;
                            },

                            // Handle messages that are simply passed through.
                            EditorMessageContents::Closed |
                            EditorMessageContents::Result(_) => {
                                debug!("Forwarding it to the IDE.");
                                queue_send!(to_ide_tx.send(client_message))
                            },

                            // Open a web browser when requested.
                            EditorMessageContents::OpenUrl(url) => {
                                if let Err(err) = open::that_detached(&url) {
                                    let msg = format!("Unable to open web browser to URL {url}: {err}");
                                    error!("{msg}");
                                    send_response(&to_client_tx, client_message.id, Err(msg)).await;
                                } else {
                                    send_response(&to_client_tx, client_message.id, Ok(ResultOkTypes::Void)).await;
                                }
                            },

                            // Handle the `Update` message.
                            EditorMessageContents::Update(update_message_contents) => {
                                debug!("Forwarding translation of it to the IDE.");
                                let codechat_for_web = match update_message_contents.contents {
                                    None => None,
                                    Some(cfw) => match codechat_for_web_to_source(
                                        &cfw)
                                    {
                                        Ok(result) => Some(CodeChatForWeb {
                                            metadata: cfw.metadata,
                                            source: CodeMirror {
                                                doc: result,
                                                doc_blocks: vec![],
                                            },
                                        }),
                                        Err(message) => {
                                            let msg = format!(
                                                "Unable to translate to source: {message}"
                                            );
                                            error!("{msg}");
                                            send_response(&to_client_tx, client_message.id, Err(msg)).await;
                                            continue;
                                        }
                                    },
                                };
                                match try_canonicalize(&update_message_contents.file_path) {
                                    Err(err) => {
                                        let msg = format!(
                                            "Unable to canonicalize file name {}: {err}", &update_message_contents.file_path
                                        );
                                        error!("{msg}");
                                        send_response(&to_client_tx, client_message.id, Err(msg)).await;
                                        continue;
                                    }
                                    Ok(clean_file_path) => {
                                        queue_send!(to_ide_tx.send(EditorMessage {
                                            id: client_message.id,
                                            message: EditorMessageContents::Update(UpdateMessageContents {
                                                file_path: clean_file_path.to_str().expect("Since the path started as a string, assume it losslessly translates back to a string.").to_string(),
                                                contents: codechat_for_web,
                                                cursor_position: update_message_contents.cursor_position,
                                                scroll_position: update_message_contents.scroll_position,
                                            })
                                        }));
                                    }
                                }
                            },

                            // Update the current file; translate it to a URL
                            // then pass it to the IDE.
                            EditorMessageContents::CurrentFile(url_string) => {
                                debug!("Forwarding translated path to IDE.");
                                let result = match url_to_path(&url_string, VSCODE_PATH_PREFIX) {
                                    Err(err) => Err(format!("Unable to convert URL to path: {err}")),
                                    Ok(file_path) => {
                                        match file_path.to_str() {
                                            None => Err("Unable to convert path to string.".to_string()),
                                            Some(file_path_string) => {
                                                queue_send!(to_ide_tx.send(EditorMessage {
                                                    id: client_message.id,
                                                    message: EditorMessageContents::CurrentFile(file_path_string.to_string())
                                                }));
                                                current_file = file_path;
                                                Ok(())
                                            }
                                        }
                                    }
                                };
                                if let Err(msg) = result {
                                    error!("{msg}");
                                    send_response(&to_client_tx, client_message.id, Err(msg)).await;
                                }
                            }
                        }
                    },

                    else => break
                }
            }

            debug!("VSCode processing task shutting down.");
            if app_state_task
                .processing_task_queue_tx
                .lock()
                .unwrap()
                .remove(&connection_id_task)
                .is_none()
            {
                error!(
                    "Unable to remove connection ID {connection_id_task} from processing task queue."
                );
            }
            if app_state_task
                .vscode_client_queues
                .lock()
                .unwrap()
                .remove(&connection_id_task)
                .is_none()
            {
                error!("Unable to remove connection ID {connection_id_task} from client queues.");
            }
            if app_state_task
                .vscode_ide_queues
                .lock()
                .unwrap()
                .remove(&connection_id_task)
                .is_none()
            {
                error!("Unable to remove connection ID {connection_id_task} from IDE queues.");
            }

            from_ide_rx.close();
            from_ide_rx.close();

            // Drain any remaining messages after closing the queue.
            while let Some(m) = from_ide_rx.recv().await {
                warn!("Dropped queued message {m:?}");
            }
            while let Some(m) = from_client_rx.recv().await {
                warn!("Dropped queued message {m:?}");
            }
            debug!("VSCode processing task exited.");
        }
    });

    // Move data between the IDE and the processing task via queues. The
    // websocket connection between the client and the IDE will run in the
    // endpoint for that connection.
    client_websocket(
        connection_id,
        req,
        body,
        app_state.vscode_ide_queues.clone(),
    )
    .await
}

pub fn get_vscode_client_framework(connection_id: &str) -> String {
    // Send the HTML for the internal browser.
    match get_client_framework(false, "vsc/ws-client", connection_id) {
        Ok(web_page) => web_page,
        Err(html_string) => {
            error!("{html_string}");
            html_wrapper(&escape_html(&html_string))
        }
    }
}

/// Serve the Client Framework.
#[get("/vsc/cf/{connection_id}")]
pub async fn vscode_client_framework(connection_id: web::Path<String>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html")
        .body(get_vscode_client_framework(&connection_id))
}

/// Define a websocket handler for the CodeChat Editor Client.
#[get("/vsc/ws-client/{connection_id}")]
pub async fn vscode_client_websocket(
    connection_id: web::Path<String>,
    req: HttpRequest,
    body: web::Payload,
    app_state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    client_websocket(
        connection_id,
        req,
        body,
        app_state.vscode_client_queues.clone(),
    )
    .await
}

// Respond to requests for the filesystem.
#[get("/vsc/fs/{connection_id}/{file_path:.*}")]
async fn serve_vscode_fs(
    request_path: web::Path<(String, String)>,
    req: HttpRequest,
    app_state: web::Data<AppState>,
) -> HttpResponse {
    filesystem_endpoint(request_path, &req, &app_state).await
}

// Tests
// -----
#[cfg(test)]
mod test {
    use std::{
        fs,
        io::Error,
        path::{self, Path, PathBuf},
        thread,
        time::{Duration, SystemTime},
    };

    use actix_rt::task::JoinHandle;
    use assert_fs::TempDir;
    use assertables::{assert_ends_with, assert_starts_with};
    use dunce::simplified;
    use futures_util::{SinkExt, StreamExt};
    use lazy_static::lazy_static;
    use minreq;
    use path_slash::PathExt;
    use tokio::{
        io::{AsyncRead, AsyncWrite},
        net::TcpStream,
        select,
        time::sleep,
    };
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, connect_async,
        tungstenite::{http::StatusCode, protocol::Message},
    };

    use super::super::{
        EditorMessage, EditorMessageContents, IP_ADDRESS, IdeType, run_server, tests::IP_PORT,
    };
    use crate::{
        cast,
        processing::{CodeChatForWeb, CodeMirror, SourceFileMetadata},
        test_utils::{_prep_test_dir, check_logger_errors, configure_testing_logger},
        webserver::{ResultOkTypes, UpdateMessageContents, drop_leading_slash},
    };

    lazy_static! {
        // Run a single webserver for all tests.
        static ref WEBSERVER_HANDLE: JoinHandle<Result<(), Error>> =
            actix_rt::spawn(async move { run_server(IP_PORT).await });
    }

    // Send a message via a websocket.
    async fn send_message<S: AsyncRead + AsyncWrite + Unpin>(
        ws_stream: &mut WebSocketStream<S>,
        message: &EditorMessage,
    ) {
        ws_stream
            .send(Message::Text(
                serde_json::to_string(message).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    // Read a message from a websocket.
    async fn read_message<S: AsyncRead + AsyncWrite + Unpin>(
        ws_stream: &mut WebSocketStream<S>,
    ) -> EditorMessage {
        let now = SystemTime::now();
        let msg_txt = loop {
            let msg = select! {
                data = ws_stream.next() => data.unwrap().unwrap(),
                _ = sleep(Duration::from_secs(3) - now.elapsed().unwrap()) => panic!("Timeout waiting for message")
            };
            match msg {
                Message::Close(_) => panic!("Unexpected close message."),
                Message::Ping(_) => ws_stream.send(Message::Pong(vec![].into())).await.unwrap(),
                Message::Pong(_) => panic!("Unexpected pong message."),
                Message::Text(txt) => break txt,
                Message::Binary(_) => panic!("Unexpected binary message."),
                Message::Frame(_) => panic!("Unexpected frame message."),
            }
        };
        serde_json::from_str(&msg_txt)
            .unwrap_or_else(|_| panic!("Unable to convert '{msg_txt}' to JSON."))
    }

    type WebSocketStreamTcp = WebSocketStream<MaybeTlsStream<TcpStream>>;

    async fn connect_async_server(prefix: &str, connection_id: &str) -> WebSocketStreamTcp {
        connect_async(format!(
            "ws://{IP_ADDRESS}:{IP_PORT}{prefix}/{connection_id}",
        ))
        .await
        .expect("Failed to connect")
        .0
    }

    async fn connect_async_ide(connection_id: &str) -> WebSocketStreamTcp {
        connect_async_server("/vsc/ws-ide", connection_id).await
    }

    async fn connect_async_client(connection_id: &str) -> WebSocketStreamTcp {
        connect_async_server("/vsc/ws-client", connection_id).await
    }

    // Open the Client in the VSCode browser. (Although, for testing, the Client
    // isn't opened at all.)
    //
    // Message ids at function end: IDE - 4, Server - 3, Client - 2.
    async fn open_client<S: AsyncRead + AsyncWrite + Unpin>(ws_ide: &mut WebSocketStream<S>) {
        // 1.  Send the `Opened` message.
        //
        // Message ids: IDE - 1->4, Server - 0, Client - 2.
        send_message(
            ws_ide,
            &EditorMessage {
                id: 1.0,
                message: EditorMessageContents::Opened(IdeType::VSCode(true)),
            },
        )
        .await;

        // Get the response. It should be success.
        assert_eq!(
            read_message(ws_ide).await,
            EditorMessage {
                id: 1.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            }
        );

        // 2.  Next, wait for the next message -- the HTML.
        //
        // Message ids: IDE - 4, Server - 0->3, Client - 2.
        let em = read_message(ws_ide).await;
        assert_starts_with!(
            cast!(&em.message, EditorMessageContents::ClientHtml),
            "<!DOCTYPE html>"
        );
        assert_eq!(em.id, 0.0);

        // Send a success response to this message.
        send_message(
            ws_ide,
            &EditorMessage {
                id: 0.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
    }

    // Perform all the setup for testing the Server via IDE and Client
    // websockets. This should be invoked by the `prep_test!` macro; otherwise,
    // test files won't be found.
    async fn _prep_test(
        connection_id: &str,
        test_full_name: &str,
    ) -> (TempDir, PathBuf, WebSocketStreamTcp, WebSocketStreamTcp) {
        configure_testing_logger();
        let (temp_dir, test_dir) = _prep_test_dir(test_full_name);
        // Ensure the webserver is running.
        let _ = &*WEBSERVER_HANDLE;
        let now = SystemTime::now();
        while now.elapsed().unwrap().as_millis() < 100 {
            if minreq::get(format!("http://{IP_ADDRESS}:{IP_PORT}/ping",))
                .send()
                .is_ok()
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // Connect to the VSCode IDE websocket.
        let ws_ide = connect_async_ide(connection_id).await;
        let ws_client = connect_async_client(connection_id).await;

        (temp_dir, test_dir, ws_ide, ws_client)
    }

    // This calls `_prep_test` with the current function name. It must be a
    // macro, so that it's called with the test function's name; calling it
    // inside `_prep_test` would give the wrong name.
    macro_rules! prep_test {
        ($connection_id: ident) => {{
            use crate::function_name;
            _prep_test($connection_id, function_name!())
        }};
    }

    // Test incorrect inputs: two connections with the same ID, sending the
    // wrong first message.
    #[actix_web::test]
    async fn test_vscode_ide_websocket1() {
        let connection_id = "test-connection-id1";
        let (_, _, mut ws_ide, _) = prep_test!(connection_id).await;

        // Start a second connection; verify that it fails.
        let err = connect_async(format!(
            "ws://{IP_ADDRESS}:{IP_PORT}/vsc/ws-ide/{connection_id}",
        ))
        .await
        .expect_err("Should fail to connect");
        let response = cast!(err, tokio_tungstenite::tungstenite::Error::Http);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Note: we can't check the logs, since the server runs in a separate
        // thread. Changing the logger to log across threads means we get logs
        // from other tests (which run in parallel by default). The benefit of
        // running all tests single-threaded plus fixing the logger is low.
        //
        // Send a message that's not an `Opened` message.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 0.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: "".to_string(),
                    contents: None,
                    cursor_position: None,
                    scroll_position: None,
                }),
            },
        )
        .await;

        // Get the response. It should be an error.
        let em = read_message(&mut ws_ide).await;
        let result = cast!(em.message, EditorMessageContents::Result);

        assert_starts_with!(cast!(&result, Err), "Unexpected message");

        // Next, expect the websocket to be closed.
        let err = &ws_ide.next().await.unwrap().unwrap();
        assert_eq!(*err, Message::Close(None));

        check_logger_errors(0);
    }

    // Test opening the Client in an external browser.
    #[actix_web::test]
    async fn test_vscode_ide_websocket2() {
        let connection_id = "test-connection-id2";
        let (_, _, mut ws_ide, _) = prep_test!(connection_id).await;

        // Send the `Opened` message.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 0.0,
                message: EditorMessageContents::Opened(IdeType::VSCode(false)),
            },
        )
        .await;

        // Get the response. It should be success.
        let em = read_message(&mut ws_ide).await;
        assert_eq!(
            cast!(em.message, EditorMessageContents::Result),
            Ok(ResultOkTypes::Void)
        );

        check_logger_errors(0);
    }

    // Fetch a non-existent file and verify the response returns an error.
    #[actix_web::test]
    async fn test_vscode_ide_websocket3() {
        let connection_id = "test-connection-id3";
        let (temp_dir, test_dir, mut ws_ide, _) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        let file_path = test_dir.join("none.py");
        let file_path_str = drop_leading_slash(&file_path.to_slash().unwrap()).to_string();

        // Do this is a thread, since the request generates a message that
        // requires a response in order to complete.
        let file_path_str_thread = file_path_str.clone();
        let join_handle = thread::spawn(move || {
            assert_eq!(
                minreq::get(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}",
                    file_path_str_thread
                ))
                .send()
                .unwrap()
                .status_code,
                404
            )
        });

        // The HTTP request produces a `LoadFile` message.
        //
        // Message ids: IDE - 4, Server - 3->6, Client - 2.
        let em = read_message(&mut ws_ide).await;
        let msg = cast!(em.message, EditorMessageContents::LoadFile);
        // Compare these as strings -- we want to ensure the path separator is
        // correct for the current platform.
        assert_eq!(file_path.to_string_lossy(), msg.to_string_lossy());
        assert_eq!(em.id, 3.0);

        // Reply to the `LoadFile` message -- the file isn't present.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 3.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::LoadFile(None))),
            },
        )
        .await;

        // This should cause the HTTP request to complete by receiving the
        // response (file not found).
        join_handle.join().unwrap();

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Fetch a file that exists, but using backslashes. This should still fail,
    // even on Windows.
    #[actix_web::test]
    async fn test_vscode_ide_websocket3a() {
        let connection_id = "test-connection-id3a";
        let (temp_dir, test_dir, mut ws_ide, _) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        let file_path = test_dir.join("test.py");
        // Force the path separator to be Window-style for this test, even on
        // non-Windows platforms.
        let file_path_str = file_path.to_str().unwrap().to_string().replace("/", "\\");

        // Do this is a thread, since the request generates a message that
        // requires a response in order to complete.
        let file_path_str_thread = file_path_str.clone();
        let join_handle = thread::spawn(move || {
            assert_eq!(
                minreq::get(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}",
                    file_path_str_thread
                ))
                .send()
                .unwrap()
                .status_code,
                404
            )
        });

        // The HTTP request produces a `LoadFile` message.
        //
        // Message ids: IDE - 4, Server - 3->6, Client - 2.
        let em = read_message(&mut ws_ide).await;
        cast!(em.message, EditorMessageContents::LoadFile);
        // Skip comparing the file names, due to the backslash encoding.
        assert_eq!(em.id, 3.0);

        // Reply to the `LoadFile` message -- the file isn't present.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 3.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::LoadFile(None))),
            },
        )
        .await;

        // This should cause the HTTP request to complete by receiving the
        // response (file not found).
        join_handle.join().unwrap();

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Send a `CurrentFile` message with a file to edit that exists only in the
    // IDE.
    #[actix_web::test]
    async fn test_vscode_ide_websocket8() {
        let connection_id = "test-connection-id8";
        let (temp_dir, test_dir, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        // Message ids: IDE - 4->7, Server - 3, Client - 2.
        let file_path = test_dir.join("only-in-ide.py");
        let file_path_str = file_path.to_str().unwrap().to_string();
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::CurrentFile(file_path_str.clone()),
            },
        )
        .await;

        // This should be passed to the Client.
        let em = read_message(&mut ws_client).await;
        assert_eq!(em.id, 4.0);
        assert_ends_with!(
            cast!(&em.message, EditorMessageContents::CurrentFile),
            "/only-in-ide.py"
        );

        // The Client should send a response.
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;

        // The IDE should receive it.
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        // The Client should send a GET request for this file.
        let file_path_thread = file_path.clone();
        let join_handle = thread::spawn(move || {
            assert_eq!(
                minreq::get(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}",
                    drop_leading_slash(&file_path_thread.to_slash().unwrap())
                ))
                .send()
                .unwrap()
                .status_code,
                200
            )
        });

        // This should produce a `LoadFile` message.
        //
        // Message ids: IDE - 7, Server - 3->6, Client - 2.
        let em = read_message(&mut ws_ide).await;
        let msg = cast!(em.message, EditorMessageContents::LoadFile);
        assert_eq!(
            path::absolute(Path::new(&msg)).unwrap(),
            path::absolute(&file_path).unwrap()
        );
        assert_eq!(em.id, 3.0);

        // Reply to the `LoadFile` message with the file's contents.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 3.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::LoadFile(Some(
                    "# testing".to_string(),
                )))),
            },
        )
        .await;
        join_handle.join().unwrap();

        // This should also produce an `Update` message sent from the Server.
        //
        // Message ids: IDE - 7, Server - 6->9, Client - 2.
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: file_path_str.clone(),
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "\n".to_string(),
                            doc_blocks: vec![(
                                0,
                                0,
                                "".to_string(),
                                "#".to_string(),
                                "<p>testing</p>\n".to_string()
                            )],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                })
            }
        );
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;

        // The message, though a result for the `Update` sent by the Server,
        // will still be echoed back to the IDE.
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Send an `Update` message from the IDE.
    #[actix_web::test]
    async fn test_vscode_ide_websocket7() {
        let connection_id = "test-connection-id7";
        let (temp_dir, test_dir, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        // Set the current file, so a subsequent `Update` message can be
        // translated.
        //
        // Message ids: IDE - 4, Server - 3, Client - 2->5.
        let file_path = test_dir.join("test.py");
        let file_path_str = file_path.to_str().unwrap().to_string();
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::CurrentFile(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}",
                    &file_path.to_slash().unwrap()
                )),
            },
        )
        .await;
        let em = read_message(&mut ws_ide).await;
        let cf = cast!(em.message, EditorMessageContents::CurrentFile);
        assert_eq!(path::absolute(Path::new(&cf)).unwrap(), file_path);
        assert_eq!(em.id, 2.0);

        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        // Send an `Update` message.
        //
        // Message ids: IDE - 4->7, Server - 3, Client - 5.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: file_path_str.clone(),
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "# more".to_string(),
                            doc_blocks: vec![],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                }),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: file_path_str.clone(),
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "\n".to_string(),
                            doc_blocks: vec![(
                                0,
                                0,
                                "".to_string(),
                                "#".to_string(),
                                "<p>more</p>\n".to_string()
                            )],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                })
            }
        );
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Send an `Update` message from the Client.
    #[actix_web::test]
    async fn test_vscode_ide_websocket6() {
        let connection_id = "test-connection-id6";
        let (temp_dir, test_dir, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        // Message ids: IDE - 4, Server - 3, Client - 2->5.
        let file_path = test_dir.join("foo.py").to_string_lossy().to_string();
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: file_path.clone(),
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "\n".to_string(),
                            doc_blocks: vec![(
                                0,
                                0,
                                "".to_string(),
                                "#".to_string(),
                                "less\n".to_string(),
                            )],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                }),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path,
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "# less\n".to_string(),
                            doc_blocks: vec![],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                })
            }
        );
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Send a `CurrentFile` message from the Client, requesting a file that
    // exists on disk, but not in the IDE.
    #[actix_web::test]
    async fn test_vscode_ide_websocket4() {
        let connection_id = "test-connection-id4";
        let (temp_dir, test_dir, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        // Message ids: IDE - 4, Server - 3, Client - 2->5.
        let file_path_temp = fs::canonicalize(test_dir.join("test.py")).unwrap();
        let file_path = simplified(&file_path_temp);
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::CurrentFile(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}",
                    &file_path.to_slash().unwrap()
                )),
            },
        )
        .await;

        let em = read_message(&mut ws_ide).await;
        let cf = cast!(em.message, EditorMessageContents::CurrentFile);
        assert_eq!(cf, file_path.to_str().unwrap().to_string());
        assert_eq!(em.id, 2.0);

        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 2.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void))
            }
        );

        // The Client should send a GET request for this file.
        let test_dir_thread = test_dir.clone();
        let join_handle = thread::spawn(move || {
            assert_eq!(
                minreq::get(format!(
                    "http://localhost:8080/vsc/fs/{connection_id}/{}/{}",
                    test_dir_thread.to_slash().unwrap(),
                    // On Windows, send incorrect case for this file; the server
                    // should correct it.
                    if cfg!(windows) { "Test.py" } else { "test.py" }
                ))
                .send()
                .unwrap()
                .status_code,
                200
            )
        });

        // This should produce a `LoadFile` message.
        //
        // Message ids: IDE - 4, Server - 3->6, Client - 5.
        let em = read_message(&mut ws_ide).await;
        let msg = cast!(em.message, EditorMessageContents::LoadFile);
        assert_eq!(fs::canonicalize(&msg).unwrap(), file_path_temp);
        assert_eq!(em.id, 3.0);

        // Reply to the `LoadFile` message: the IDE doesn't have the file.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 3.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::LoadFile(None))),
            },
        )
        .await;
        join_handle.join().unwrap();

        // This should also produce an `Update` message sent from the Server.
        //
        // Message ids: IDE - 4, Server - 6->9, Client - 5.
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Update(UpdateMessageContents {
                    file_path: file_path.to_str().unwrap().to_string(),
                    contents: Some(CodeChatForWeb {
                        metadata: SourceFileMetadata {
                            mode: "python".to_string(),
                        },
                        source: CodeMirror {
                            doc: "\n".to_string(),
                            doc_blocks: vec![(
                                0,
                                0,
                                "".to_string(),
                                "#".to_string(),
                                "<p>test.py</p>\n".to_string()
                            )],
                        },
                    }),
                    cursor_position: None,
                    scroll_position: None,
                })
            }
        );
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 6.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            }
        );

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Send a `RequestClose` message to the Client, then close the Client.
    #[actix_web::test]
    async fn test_vscode_ide_websocket5() {
        let connection_id = "test-connection-id5";
        let (temp_dir, _, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        // Message ids: IDE - 4->7, Server - 3, Client - 2.
        //
        // Send the `RequestClose` message.
        send_message(
            &mut ws_ide,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::RequestClose,
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_client).await,
            EditorMessage {
                id: 4.0,
                message: EditorMessageContents::RequestClose
            }
        );
        send_message(
            &mut ws_client,
            &EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            },
        )
        .await;
        assert_eq!(
            read_message(&mut ws_ide).await,
            EditorMessage {
                id: 4.0,
                message: EditorMessageContents::Result(Ok(ResultOkTypes::Void)),
            }
        );

        // Close the Client websocket.
        ws_client.close(None).await.unwrap();
        loop {
            match ws_ide.next().await.unwrap().unwrap() {
                Message::Ping(_) => ws_ide.send(Message::Pong(vec![].into())).await.unwrap(),
                Message::Close(_) => break,
                _ => panic!("Unexpected message."),
            }
        }

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }

    // Close the IDE.
    #[actix_web::test]
    async fn test_vscode_ide_websocket9() {
        let connection_id = "test-connection-id9";
        let (temp_dir, _, mut ws_ide, mut ws_client) = prep_test!(connection_id).await;
        open_client(&mut ws_ide).await;

        ws_ide.close(None).await.unwrap();
        loop {
            match ws_client.next().await.unwrap().unwrap() {
                Message::Ping(_) => ws_client.send(Message::Pong(vec![].into())).await.unwrap(),
                Message::Close(_) => break,
                _ => panic!("Unexpected message."),
            }
        }

        check_logger_errors(0);
        // Report any errors produced when removing the temporary directory.
        temp_dir.close().unwrap();
    }
}
