use tower_lsp_server::{
    LanguageServer, LspService, Server,
    jsonrpc::Result,
    ls_types::{InitializeParams, InitializeResult},
};

// This backend implements the required language-server lifecycle without advertising features.
#[derive(Debug)]
struct Backend;

// Respond to the protocol's required requests while leaving all optional capabilities disabled.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "The no-op methods mirror the asynchronous language-server interface."
)]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult::default())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

// Serve language-server requests over standard input and output until the client disconnects.
pub async fn run() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|_client| Backend);
    Server::new(stdin, stdout, socket).serve(service).await;
}
