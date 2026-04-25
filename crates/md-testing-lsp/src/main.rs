use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod diagnostics;
mod results;

use diagnostics::build_diagnostics;
use results::ResultsStore;

#[derive(Debug)]
struct MdTestingLsp {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, String>>>,
    results_store: Arc<ResultsStore>,
}

#[tower_lsp::async_trait]
impl LanguageServer for MdTestingLsp {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("md-testing".into()),
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(false),
                        },
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "md-testing-lsp initialized")
            .await;

        // Start watching the results directory
        let client = self.client.clone();
        let store = self.results_store.clone();
        let docs = self.documents.clone();
        tokio::spawn(async move {
            store.watch_results_dir(client, docs).await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let content = params.text_document.text;

        self.documents
            .write()
            .await
            .insert(uri.clone(), content.clone());

        if is_testcase_file(&uri) {
            self.publish_diagnostics(&uri, &content).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next() {
            let content = change.text;
            self.documents
                .write()
                .await
                .insert(uri.clone(), content.clone());

            if is_testcase_file(&uri) {
                self.publish_diagnostics(&uri, &content).await;
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if is_testcase_file(&uri) {
            // Refresh results and re-publish
            if let Some(content) = self.documents.read().await.get(&uri) {
                self.publish_diagnostics(&uri, content).await;
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .await
            .remove(&params.text_document.uri);
    }
}

impl MdTestingLsp {
    async fn publish_diagnostics(&self, uri: &Url, content: &str) {
        let diagnostics = build_diagnostics(content, uri, &self.results_store).await;

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

fn is_testcase_file(uri: &Url) -> bool {
    uri.path().ends_with(".testcase.md")
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let results_store = Arc::new(ResultsStore::new());
        MdTestingLsp {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            results_store,
        }
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
