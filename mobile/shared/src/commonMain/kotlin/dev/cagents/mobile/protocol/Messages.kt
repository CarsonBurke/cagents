package dev.cagents.mobile.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

// The server-to-client messages of docs/mobile-api.md, API version 1. The
// definitions mirror crates/mobile-api/src/lib.rs. Every field the server may
// leave out has a default, and every enum decodes a value it doesn't know to
// its Unknown case.

const val API_VERSION = 1
const val MIN_API_VERSION = 1

@Serializable
data class Welcome(
    val api: Int,
    @SerialName("server_version") val serverVersion: String,
    val host: String? = null,
)

@Serializable
data class RemoteError(
    val code: ErrorCode = ErrorCode.Unknown,
    val message: String = "",
)

@Serializable(with = ErrorCode.Serializer::class)
enum class ErrorCode(override val wire: String) : WireEnum {
    BadRequest("bad_request"),
    UnsupportedApi("unsupported_api"),
    UnknownOp("unknown_op"),
    NotFound("not_found"),
    NotWatching("not_watching"),
    NotWatchable("not_watchable"),
    NotAllowed("not_allowed"),
    Unavailable("unavailable"),
    Internal("internal"),
    Unknown("");

    object Serializer : WireEnumSerializer<ErrorCode>("ErrorCode", entries, Unknown)
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

@Serializable
data class SessionList(val sessions: List<Session> = emptyList())

@Serializable
data class Session(
    val id: String,
    val lineage: String = "",
    val surface: Surface = Surface.Unknown,
    val provider: String? = null,
    @SerialName("provider_label") val providerLabel: String? = null,
    val title: String? = null,
    val project: String? = null,
    val directory: String? = null,
    val activity: Activity = Activity.Unknown,
    val exited: Boolean = false,
    val failure: String? = null,
    @SerialName("active_at") val activeAt: Long? = null,
    val agent: Agent? = null,
    val watchable: Boolean = false,
)

@Serializable(with = Surface.Serializer::class)
enum class Surface(override val wire: String) : WireEnum {
    Chat("chat"),
    Terminal("terminal"),
    Unknown("");

    object Serializer : WireEnumSerializer<Surface>("Surface", entries, Unknown)
}

@Serializable(with = Activity.Serializer::class)
enum class Activity(override val wire: String) : WireEnum {
    Idle("idle"),
    Working("working"),
    /** It needs the person: an approval, a question, or a finished turn awaiting an answer. */
    Waiting("waiting"),
    Unknown("unknown");

    object Serializer : WireEnumSerializer<Activity>("Activity", entries, Unknown)
}

@Serializable
data class Agent(
    val id: String,
    val owner: AgentOwner = AgentOwner.Unknown,
    @SerialName("owner_lineage") val ownerLineage: String? = null,
    val run: RunState? = null,
)

@Serializable(with = AgentOwner.Serializer::class)
enum class AgentOwner(override val wire: String) : WireEnum {
    User("user"),
    Session("session"),
    Automation("automation"),
    Unknown("");

    object Serializer : WireEnumSerializer<AgentOwner>("AgentOwner", entries, Unknown)
}

@Serializable(with = RunState.Serializer::class)
enum class RunState(override val wire: String) : WireEnum {
    Accepted("accepted"),
    Running("running"),
    Waiting("waiting"),
    Succeeded("succeeded"),
    Failed("failed"),
    Cancelled("cancelled"),
    Interrupted("interrupted"),
    Unknown("");

    object Serializer : WireEnumSerializer<RunState>("RunState", entries, Unknown)
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

@Serializable
data class ConversationUpdate(
    val session: String,
    val reset: Boolean = false,
    val state: ConversationState = ConversationState(),
    val order: List<String>? = null,
    val items: List<Item> = emptyList(),
    val appends: List<TextAppend> = emptyList(),
)

@Serializable
data class TextAppend(
    val id: String,
    /** The held text's length in UTF-16 code units, which is what a Kotlin String counts. */
    val offset: Long,
    val text: String,
    val at: Long? = null,
)

@Serializable
data class ConversationState(
    val title: String? = null,
    val activity: Activity = Activity.Unknown,
    val busy: Boolean = false,
    val ready: Boolean = false,
    val exited: Boolean = false,
    val error: String? = null,
    @SerialName("read_only") val readOnly: String? = null,
    @SerialName("has_earlier") val hasEarlier: Boolean = false,
    @SerialName("loading_earlier") val loadingEarlier: Boolean = false,
    val model: String? = null,
    val requests: List<PendingRequest> = emptyList(),
    val queued: List<QueuedMessage> = emptyList(),
    val plan: List<PlanStep> = emptyList(),
    val subagents: List<Subagent> = emptyList(),
)

@Serializable
data class QueuedMessage(val id: String, val text: String = "")

@Serializable
data class PlanStep(val text: String = "", val status: PlanStatus = PlanStatus.Unknown)

@Serializable(with = PlanStatus.Serializer::class)
enum class PlanStatus(override val wire: String) : WireEnum {
    Pending("pending"),
    Active("active"),
    Done("done"),
    Unknown("");

    object Serializer : WireEnumSerializer<PlanStatus>("PlanStatus", entries, Unknown)
}

@Serializable
data class Subagent(
    val id: String,
    val kind: String = "",
    val description: String = "",
    val status: ToolStatus = ToolStatus.Unknown,
    val waiting: Boolean = false,
)

@Serializable
data class PendingRequest(
    val id: String,
    val kind: RequestKind = RequestKind.Unknown,
    val title: String = "",
    val description: String = "",
    val fields: List<RequestField> = emptyList(),
    /** There is no implicit default: an approval is given only by choosing one. */
    val actions: List<Choice> = emptyList(),
)

@Serializable(with = RequestKind.Serializer::class)
enum class RequestKind(override val wire: String) : WireEnum {
    Approval("approval"),
    Question("question"),
    Consent("consent"),
    Unknown("");

    object Serializer : WireEnumSerializer<RequestKind>("RequestKind", entries, Unknown)
}

@Serializable
data class RequestField(
    val id: String,
    val label: String = "",
    val description: String = "",
    /** Offered values; free text when empty. */
    val choices: List<Choice> = emptyList(),
    val multiple: Boolean = false,
    val secret: Boolean = false,
    val initial: String = "",
)

@Serializable
data class Choice(val id: String, val label: String = "", val description: String = "")

// ---------------------------------------------------------------------------
// Transcript items
// ---------------------------------------------------------------------------

/** A transcript item; [body] holds what its `kind` carries. See [ItemSerializer]. */
@Serializable(with = ItemSerializer::class)
data class Item(
    val id: String,
    val role: Role,
    val body: ItemBody,
    val startedAt: Long? = null,
    val at: Long? = null,
)

@Serializable(with = Role.Serializer::class)
enum class Role(override val wire: String) : WireEnum {
    User("user"),
    Assistant("assistant"),
    System("system"),
    Unknown("");

    object Serializer : WireEnumSerializer<Role>("Role", entries, Unknown)
}

sealed interface ItemBody {
    @Serializable
    data class Text(
        val text: String = "",
        /** The model's reasoning rather than what it said. */
        val thinking: Boolean = false,
        /** Narration while the turn works, rather than its answer. */
        val commentary: Boolean = false,
    ) : ItemBody

    @Serializable
    data class Command(
        val label: String = "",
        val command: String = "",
        val action: CommandAction = CommandAction.Unknown,
        val status: ToolStatus = ToolStatus.Unknown,
        @SerialName("exit_code") val exitCode: Int? = null,
        @SerialName("duration_ms") val durationMs: Long? = null,
        val output: String = "",
        val clipped: Boolean = false,
    ) : ItemBody

    @Serializable
    data class Changes(
        val files: List<FileChange> = emptyList(),
        val status: ToolStatus = ToolStatus.Unknown,
    ) : ItemBody

    @Serializable
    data class WebSearch(
        val query: String = "",
        val results: List<SearchResult> = emptyList(),
    ) : ItemBody

    @Serializable
    data class Tool(
        val name: String = "",
        val input: String = "",
        val output: String = "",
        val clipped: Boolean = false,
        val status: ToolStatus = ToolStatus.Unknown,
    ) : ItemBody

    @Serializable
    data class Image(val description: String = "") : ItemBody

    @Serializable
    data class Delivery(
        val delivery: DeliveryKind = DeliveryKind.Unknown,
        val from: String = "",
        val summary: String = "",
        val body: String = "",
        val clipped: Boolean = false,
    ) : ItemBody

    /** A kind this app doesn't know, from a newer server; shown as a placeholder. */
    data class Unknown(val kind: String) : ItemBody
}

@Serializable(with = ToolStatus.Serializer::class)
enum class ToolStatus(override val wire: String) : WireEnum {
    Running("running"),
    Succeeded("succeeded"),
    Failed("failed"),
    Denied("denied"),
    Stopped("stopped"),
    /** Stopped with background work of its own still running. */
    Waiting("waiting"),
    /** Superseded, with nothing left to wait for. */
    Ended("ended"),
    Unknown("");

    object Serializer : WireEnumSerializer<ToolStatus>("ToolStatus", entries, Unknown)
}

@Serializable(with = CommandAction.Serializer::class)
enum class CommandAction(override val wire: String) : WireEnum {
    Run("run"),
    Read("read"),
    Search("search"),
    List("list"),
    Unknown("");

    object Serializer : WireEnumSerializer<CommandAction>("CommandAction", entries, Unknown)
}

@Serializable
data class FileChange(
    val path: String,
    val change: ChangeKind = ChangeKind.Unknown,
    val from: String? = null,
    @SerialName("added_lines") val addedLines: Long = 0,
    @SerialName("removed_lines") val removedLines: Long = 0,
)

@Serializable(with = ChangeKind.Serializer::class)
enum class ChangeKind(override val wire: String) : WireEnum {
    Added("added"),
    Deleted("deleted"),
    Updated("updated"),
    Renamed("renamed"),
    Unknown("");

    object Serializer : WireEnumSerializer<ChangeKind>("ChangeKind", entries, Unknown)
}

@Serializable
data class SearchResult(val title: String = "", val url: String = "")

@Serializable(with = DeliveryKind.Serializer::class)
enum class DeliveryKind(override val wire: String) : WireEnum {
    Report("report"),
    Message("message"),
    Notice("notice"),
    Unknown("");

    object Serializer : WireEnumSerializer<DeliveryKind>("DeliveryKind", entries, Unknown)
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

@Serializable
data class NoticeList(val notices: List<Notice> = emptyList())

@Serializable
data class Notice(
    val id: String,
    val kind: NoticeKind = NoticeKind.Unknown,
    val asks: Boolean = false,
    val title: String = "",
    val session: String? = null,
    val conversation: String? = null,
    val preview: String? = null,
    val request: RequestKind? = null,
    val provider: String? = null,
)

@Serializable(with = NoticeKind.Serializer::class)
enum class NoticeKind(override val wire: String) : WireEnum {
    Succeeded("succeeded"),
    Failed("failed"),
    Cancelled("cancelled"),
    Waiting("waiting"),
    ReviewRequested("review_requested"),
    Interrupted("interrupted"),
    MessageReceived("message_received"),
    AutomationPaused("automation_paused"),
    AutomationBlocked("automation_blocked"),
    Unknown("");

    object Serializer : WireEnumSerializer<NoticeKind>("NoticeKind", entries, Unknown)
}
