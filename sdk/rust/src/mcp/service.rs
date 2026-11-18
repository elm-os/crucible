//! [`CrucibleService`]: a `ServerHandler` that forwards every method of the handler it wraps, recording one
//! span per request and notification.
//!
//! It forwards each method of rmcp 3.11's `ServerHandler`. rmcp gives every method a default, so a method this
//! wrapper did not forward would silently answer with the default instead of the wrapped handler; that is why
//! the crate supports one rmcp minor line at a time.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry::context::FutureExt as _;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CancelTaskParams, CancelledNotificationParam,
    CompleteRequestParams, CompleteResult, CustomNotification, CustomRequest, CustomResult,
    DiscoverResult, ErrorData, GetPromptRequestParams, GetPromptResponse, GetSkillRequestParams,
    GetSkillResult, GetTaskParams, GetTaskResult, InitializeRequestParams, InitializeResult,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListSkillsResult,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProtocolVersion,
    ReadResourceDirectoryRequestParams, ReadResourceDirectoryResult, ReadResourceRequestParams,
    ReadResourceResponse, ServerConfig, SubscribeRequestParams, SubscriptionFilter, Tool,
    UnsubscribeRequestParams, UpdateTaskParams,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer, SubscriptionContext};

use super::Config;
use super::op::{Op, Outcomes, Start};
use crate::keys::mcp as k;

/// An MCP server handler wrapped by [`super::Instrumentation::wrap`]. It implements [`ServerHandler`], and so
/// rmcp's `Service<RoleServer>`, by forwarding every method to the handler it wraps.
pub struct CrucibleService<S> {
    inner: S,
    config: Arc<Config>,
    /// The server's name and version.
    server: (String, String),
    /// The protocol version of the last request. A session without `initialize` (the modern era on stdio)
    /// stores no version for its notifications to read.
    version: Mutex<Option<String>>,
}

impl<S: fmt::Debug> fmt::Debug for CrucibleService<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrucibleService")
            .field("inner", &self.inner)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl<S: ServerHandler> CrucibleService<S> {
    pub(super) fn new(inner: S, config: Arc<Config>) -> Self {
        let options = &config.options;
        let mut server = (options.server_name.clone(), options.server_version.clone());
        if server.0.is_empty() || server.1.is_empty() {
            let declared = inner.get_info().server_info;
            if server.0.is_empty() {
                server.0 = declared.name;
            }
            if server.1.is_empty() {
                server.1 = declared.version;
            }
        }
        Self {
            inner,
            config,
            server,
            version: Mutex::new(None),
        }
    }
}

impl<S> CrucibleService<S> {
    /// The wrapped handler.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Unwraps the handler.
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn remember(&self, version: Option<String>) {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = version;
    }

    fn remembered(&self) -> Option<String> {
        self.version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn start<'a>(
        &self,
        method: &'a str,
        target: Option<(&'static str, &'a str)>,
        state_in: Option<&'a str>,
        context: &'a RequestContext<RoleServer>,
    ) -> Option<Op> {
        // An initialize request names the version the client wants; the version of the session is the one
        // the handshake settles on, which `initialize` records once its handler returns.
        let version = (method != "initialize")
            .then(|| context.protocol_version().map(|v| v.as_str().to_owned()))
            .flatten();
        if version.is_some() {
            self.remember(version.clone());
        }
        let start = Start {
            method,
            target,
            request_id: Some(context.id.to_string()),
            version,
            client: context.client_info().map(|i| (i.name, i.version)),
            meta: &context.meta.0.0,
            extensions: &context.extensions,
            state_in,
        };
        Op::begin(&self.config, &self.server, &start)
    }

    /// Runs a request's handler under its span and ends the span with the outcome.
    async fn traced<T, F>(
        &self,
        op: Option<Op>,
        mut context: RequestContext<RoleServer>,
        call: impl FnOnce(RequestContext<RoleServer>) -> F,
    ) -> Result<T, ErrorData>
    where
        T: Outcomes,
        F: Future<Output = Result<T, ErrorData>>,
    {
        let Some(op) = op else {
            return call(context).await;
        };
        context.extensions.insert(op.handle());
        let cancelled = context.ct.clone();
        let result = call(context).with_context(op.context()).await;
        op.finish(&result, cancelled.is_cancelled());
        result
    }

    /// Runs a notification's handler under its span.
    async fn notified<F>(
        &self,
        method: &str,
        context: NotificationContext<RoleServer>,
        call: impl FnOnce(NotificationContext<RoleServer>) -> F,
    ) where
        F: Future<Output = ()>,
    {
        let header = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.headers.get("mcp-protocol-version"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let peer = context.peer.peer_info();
        let version = header
            .or_else(|| {
                peer.as_ref()
                    .map(|p| p.protocol_version.as_str().to_owned())
            })
            .or_else(|| self.remembered());
        let start = Start {
            method,
            target: None,
            request_id: None,
            version,
            client: peer.map(|p| (p.client_info.name.clone(), p.client_info.version.clone())),
            meta: &context.meta.0.0,
            extensions: &context.extensions,
            state_in: None,
        };
        match Op::begin(&self.config, &self.server, &start) {
            Some(op) => {
                call(context).with_context(op.context()).await;
                op.finish_notification();
            }
            None => call(context).await,
        }
    }
}

/// Forwards a request method whose span needs nothing from its parameters.
macro_rules! request {
    ($name:ident, $method:literal, $params:ty, $out:ty) => {
        async fn $name(
            &self,
            request: $params,
            context: RequestContext<RoleServer>,
        ) -> Result<$out, ErrorData> {
            let op = self.start($method, None, None, &context);
            self.traced(op, context, move |cx| self.inner.$name(request, cx))
                .await
        }
    };
    ($name:ident, $method:literal, $out:ty) => {
        async fn $name(&self, context: RequestContext<RoleServer>) -> Result<$out, ErrorData> {
            let op = self.start($method, None, None, &context);
            self.traced(op, context, move |cx| self.inner.$name(cx))
                .await
        }
    };
}

/// Forwards a notification method.
macro_rules! notification {
    ($name:ident, $method:literal, $params:ty) => {
        async fn $name(&self, notification: $params, context: NotificationContext<RoleServer>) {
            self.notified($method, context, move |cx| {
                self.inner.$name(notification, cx)
            })
            .await;
        }
    };
    ($name:ident, $method:literal) => {
        async fn $name(&self, context: NotificationContext<RoleServer>) {
            self.notified($method, context, move |cx| self.inner.$name(cx))
                .await;
        }
    };
}

impl<S: ServerHandler> ServerHandler for CrucibleService<S> {
    request!(ping, "ping", ());
    request!(discover, "server/discover", DiscoverResult);
    request!(
        complete,
        "completion/complete",
        CompleteRequestParams,
        CompleteResult
    );
    #[expect(
        deprecated,
        reason = "the wrapped handler may still serve the legacy method"
    )]
    async fn set_level(
        &self,
        request: rmcp::model::SetLevelRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let op = self.start("logging/setLevel", None, None, &context);
        self.traced(op, context, move |cx| self.inner.set_level(request, cx))
            .await
    }
    request!(
        list_prompts,
        "prompts/list",
        Option<PaginatedRequestParams>,
        ListPromptsResult
    );
    request!(
        list_resources,
        "resources/list",
        Option<PaginatedRequestParams>,
        ListResourcesResult
    );
    request!(
        list_resource_templates,
        "resources/templates/list",
        Option<PaginatedRequestParams>,
        ListResourceTemplatesResult
    );
    request!(
        list_tools,
        "tools/list",
        Option<PaginatedRequestParams>,
        ListToolsResult
    );
    request!(get_task, "tasks/get", GetTaskParams, GetTaskResult);
    request!(update_task, "tasks/update", UpdateTaskParams, ());
    request!(cancel_task, "tasks/cancel", CancelTaskParams, ());
    request!(
        list_skills,
        "skills/list",
        Option<PaginatedRequestParams>,
        ListSkillsResult
    );
    request!(
        get_skill,
        "skills/get",
        GetSkillRequestParams,
        GetSkillResult
    );
    request!(
        read_resource_directory,
        "resources/directory/read",
        ReadResourceDirectoryRequestParams,
        ReadResourceDirectoryResult
    );

    notification!(
        on_cancelled,
        "notifications/cancelled",
        CancelledNotificationParam
    );
    notification!(
        on_progress,
        "notifications/progress",
        ProgressNotificationParam
    );
    notification!(on_initialized, "notifications/initialized");
    notification!(on_roots_list_changed, "notifications/roots/list_changed");

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        mut context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let requested = request.protocol_version.clone();
        let Some(op) = self.start("initialize", None, None, &context) else {
            return self.inner.initialize(request, context).await;
        };
        context.extensions.insert(op.handle());
        let cancelled = context.ct.clone();
        let result = self
            .inner
            .initialize(request, context)
            .with_context(op.context())
            .await;
        if let Ok(offered) = &result {
            let version = negotiated(
                &requested,
                &offered.protocol_version,
                &self.inner.supported_protocol_versions(),
            );
            op.set_version(version.as_str());
            self.remember(Some(version.as_str().to_owned()));
        }
        op.finish(&result, cancelled.is_cancelled());
        result
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let target = Some((k::TOOL_NAME, request.name.as_ref()));
        let Some(mut op) = self.start(
            "tools/call",
            target,
            request.request_state.as_deref(),
            &context,
        ) else {
            return self.inner.call_tool(request, context).await;
        };
        op.tool_call(&request);
        context.extensions.insert(op.handle());
        let cancelled = context.ct.clone();
        let result = self
            .inner
            .call_tool(request, context)
            .with_context(op.context())
            .await;
        if let Ok(CallToolResponse::Complete(complete)) = &result {
            op.tool_result(complete);
        }
        op.finish(&result, cancelled.is_cancelled());
        result
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let op = self.start(
            "prompts/get",
            Some((k::PROMPT_NAME, &request.name)),
            request.request_state.as_deref(),
            &context,
        );
        self.traced(op, context, move |cx| self.inner.get_prompt(request, cx))
            .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        // A resource URI is not a target name: only its template would be, and it is not visible here.
        let op = self.start(
            "resources/read",
            None,
            request.request_state.as_deref(),
            &context,
        );
        self.traced(op, context, move |cx| self.inner.read_resource(request, cx))
            .await
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        let method = request.method.clone();
        let op = self.start(&method, None, None, &context);
        self.traced(op, context, move |cx| {
            self.inner.on_custom_request(request, cx)
        })
        .await
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleServer>,
    ) {
        let method = notification.method.clone();
        self.notified(&method, context, move |cx| {
            self.inner.on_custom_notification(notification, cx)
        })
        .await;
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let request = context.request_context();
        let op = self.start("subscriptions/listen", None, None, request);
        let cancelled = request.ct.clone();
        let Some(op) = op else {
            return self.inner.listen(context).await;
        };
        let result = self.inner.listen(context).with_context(op.context()).await;
        op.finish(&result, cancelled.is_cancelled());
        result
    }

    #[expect(
        deprecated,
        reason = "the wrapped handler may still serve the legacy method"
    )]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let op = self.start("resources/subscribe", None, None, &context);
        self.traced(op, context, move |cx| self.inner.subscribe(request, cx))
            .await
    }

    #[expect(
        deprecated,
        reason = "the wrapped handler may still serve the legacy method"
    )]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let op = self.start("resources/unsubscribe", None, None, &context);
        self.traced(op, context, move |cx| self.inner.unsubscribe(request, cx))
            .await
    }

    fn negotiate_initialize(
        &self,
        request: &InitializeRequestParams,
    ) -> Result<InitializeResult, ErrorData> {
        self.inner.negotiate_initialize(request)
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        self.inner.supported_protocol_versions()
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        self.inner.accepted_subscription_filter(requested)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.get_tool(name)
    }

    fn get_info(&self) -> ServerConfig {
        self.inner.get_info()
    }
}

/// The version an `initialize` handshake settles on, by rmcp's rule: the requested version when the server can
/// serve it over the handshake, else the version the handler offered when it has a handshake, else the newest
/// supported version that has one.
fn negotiated(
    requested: &ProtocolVersion,
    offered: &ProtocolVersion,
    supported: &[ProtocolVersion],
) -> ProtocolVersion {
    if requested.has_initialize() && supported.contains(requested) {
        return requested.clone();
    }
    if offered.has_initialize() {
        return offered.clone();
    }
    supported
        .iter()
        .filter(|v| v.has_initialize())
        .max_by(|a, b| a.as_str().cmp(b.as_str()))
        .unwrap_or(offered)
        .clone()
}
