package dev.dbdelve.android

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.dbdelve.android.data.Profile
import dev.dbdelve.android.data.ProfileStore
import dev.dbdelve.android.data.Secrets
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.dbdelve_core.CatalogEntry
import uniffi.dbdelve_core.CoreException
import uniffi.dbdelve_core.Destructive
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.ResultSet
import uniffi.dbdelve_core.RunOutcome
import uniffi.dbdelve_core.Session
import uniffi.dbdelve_core.Stop
import uniffi.dbdelve_core.Verdict
import uniffi.dbdelve_core.statementAt

class QueryTab(val id: Long, title: String, text: String) {
    var title by mutableStateOf(title)
    var text by mutableStateOf(TextFieldValue(text, TextRange(text.length)))
    var result by mutableStateOf<ResultSet?>(null)
    var error by mutableStateOf<String?>(null)
    /** What was sent, exactly, so the results say what produced them. */
    var ran by mutableStateOf<String?>(null)
}

/** A statement the gate stopped, held while the user answers. Nothing was sent. */
data class PendingGate(
    val tab: QueryTab,
    val statement: String,
    val stop: Stop,
    val verdict: Verdict,
    val confirmed: List<Destructive>,
    val onceIn: Mode?,
)

sealed interface Screen {
    data object Connections : Screen
    data class Edit(val profile: Profile?, val duplicate: Boolean = false) : Screen
    data object Workspace : Screen
}

enum class Pane { QUERY, RESULTS, SCHEMA }

fun Throwable.reason(): String = when (this) {
    is CoreException.Connect -> reason
    is CoreException.Query -> reason
    else -> message ?: toString()
}

class AppViewModel(
    private val store: ProfileStore,
    private val secrets: Secrets,
) : ViewModel() {
    var profiles by mutableStateOf(store.load())
        private set
    var screen by mutableStateOf<Screen>(Screen.Connections)
    var connecting by mutableStateOf<String?>(null)
        private set
    var connectError by mutableStateOf<String?>(null)

    var session by mutableStateOf<Session?>(null)
        private set
    var profile by mutableStateOf<Profile?>(null)
        private set
    var mode by mutableStateOf(Mode.READ_ONLY)
        private set

    val tabs = mutableStateListOf<QueryTab>()
    var current by mutableStateOf(0)
    var pane by mutableStateOf(Pane.QUERY)
    var running by mutableStateOf(false)
        private set
    var rowLimit by mutableStateOf(1000)

    var catalog by mutableStateOf<List<CatalogEntry>>(emptyList())
        private set
    var catalogError by mutableStateOf<String?>(null)
        private set
    var gate by mutableStateOf<PendingGate?>(null)
        private set

    private var nextTabId = 0L

    val tab: QueryTab? get() = tabs.getOrNull(current)

    fun saveProfile(profile: Profile, password: String?) {
        // The password is written first: a profile saved without its password
        // would connect with a blank one, which is valid, and wrong.
        password?.let { secrets.setPassword(profile.id, it) }
        val next = profiles.filterNot { it.id == profile.id } + profile
        profiles = next.sortedWith(compareBy({ it.project }, { it.name }))
        store.save(profiles)
        screen = Screen.Connections
    }

    fun password(profile: Profile): String = secrets.password(profile.id)

    fun deleteProfile(profile: Profile) {
        profiles = profiles.filterNot { it.id == profile.id }
        store.save(profiles)
        secrets.forget(profile.id)
    }

    fun connect(profile: Profile) {
        if (connecting != null) return
        connecting = profile.id
        connectError = null
        viewModelScope.launch {
            val opened = withContext(Dispatchers.IO) {
                runCatching { Session.connect(profile.config(secrets.password(profile.id)), profile.mode) }
            }
            connecting = null
            opened.onSuccess { opened ->
                session = opened
                this@AppViewModel.profile = profile
                mode = profile.mode
                tabs.clear()
                newTab()
                pane = Pane.QUERY
                screen = Screen.Workspace
                refreshCatalog()
            }.onFailure { connectError = "${profile.name}: ${it.reason()}" }
        }
    }

    fun disconnect() {
        val closing = session
        session = null
        profile = null
        catalog = emptyList()
        gate = null
        screen = Screen.Connections
        viewModelScope.launch(Dispatchers.IO) { closing?.close() }
    }

    fun newTab(title: String = "Query ${nextTabId + 1}", text: String = ""): QueryTab {
        val tab = QueryTab(nextTabId++, title, text)
        tabs.add(tab)
        current = tabs.lastIndex
        return tab
    }

    fun closeTab(tab: QueryTab) {
        tabs.remove(tab)
        if (tabs.isEmpty()) newTab()
        current = current.coerceIn(0, tabs.lastIndex)
    }

    /** Runs the statement under the cursor, as typed. */
    fun runAtCursor() {
        val tab = tab ?: return
        val session = session ?: return
        val text = tab.text.text
        val span = statementAt(session.engine(), text, tab.text.selection.start.toUInt()) ?: return
        execute(tab, text.substring(span.start.toInt(), span.end.toInt()), emptyList(), runOnce = false, onceIn = null)
    }

    private fun execute(
        tab: QueryTab,
        statement: String,
        confirmed: List<Destructive>,
        runOnce: Boolean,
        onceIn: Mode?,
    ) {
        val session = session ?: return
        if (running) return
        running = true
        val limit = rowLimit
        // ponytail: runs in the view model's scope, so Android may kill a long
        // statement once the app leaves the screen. A foreground service that
        // owns the session is the upgrade path.
        viewModelScope.launch {
            val outcome = withContext(Dispatchers.IO) {
                runCatching { session.run(statement, limit.toUInt(), confirmed, runOnce, onceIn) }
            }
            running = false
            outcome.onSuccess { outcome ->
                when (outcome) {
                    is RunOutcome.Done -> {
                        tab.result = outcome.result
                        tab.error = null
                        tab.ran = statement
                        pane = Pane.RESULTS
                    }
                    is RunOutcome.Stopped -> gate = PendingGate(tab, statement, outcome.stop, outcome.verdict, confirmed, onceIn)
                }
            }.onFailure {
                tab.error = it.reason()
                tab.ran = statement
                pane = Pane.RESULTS
            }
        }
    }

    fun dismissGate() {
        gate = null
    }

    /** Runs the stopped statement once in [mode]; the connection's own mode stays. */
    fun runOnceIn(mode: Mode) {
        val pending = gate ?: return
        gate = null
        execute(pending.tab, pending.statement, pending.confirmed, runOnce = false, onceIn = mode)
    }

    fun confirm(kind: Destructive) {
        val pending = gate ?: return
        gate = null
        execute(pending.tab, pending.statement, pending.confirmed + kind, runOnce = false, onceIn = pending.onceIn)
    }

    fun runUnreadableOnce() {
        val pending = gate ?: return
        gate = null
        execute(pending.tab, pending.statement, pending.confirmed, runOnce = true, onceIn = pending.onceIn)
    }

    /** Switches the connection, then runs what was stopped. */
    fun switchAndRun(mode: Mode) {
        val pending = gate ?: return
        gate = null
        setMode(mode) { execute(pending.tab, pending.statement, pending.confirmed, runOnce = false, onceIn = null) }
    }

    /** The only place a connection's mode changes, and it is saved with it. */
    fun setMode(next: Mode, then: () -> Unit = {}) {
        val session = session ?: return
        val profile = profile ?: return
        viewModelScope.launch {
            val changed = withContext(Dispatchers.IO) { runCatching { session.setMode(next) } }
            changed.onSuccess {
                mode = next
                val saved = profile.copy(mode = next)
                this@AppViewModel.profile = saved
                profiles = profiles.map { if (it.id == saved.id) saved else it }
                store.save(profiles)
                then()
            }.onFailure { failure -> tab?.let { it.error = failure.reason() } }
        }
    }

    fun cancel() {
        val session = session ?: return
        // A thread of its own: the session's lock is held by what it stops.
        Thread { session.cancel() }.start()
    }

    fun refreshCatalog() {
        val session = session ?: return
        viewModelScope.launch {
            val listed = withContext(Dispatchers.IO) { runCatching { session.catalog() } }
            listed.onSuccess {
                catalog = it
                catalogError = null
            }.onFailure { catalogError = it.reason() }
        }
    }

    /** DBDelve's own preview query, in a tab of its own where it can be read. */
    fun preview(entry: CatalogEntry) {
        val session = session ?: return
        val text = session.preview(entry.schema, entry.name, rowLimit.toUInt())
        val tab = newTab(entry.name, text)
        execute(tab, text, emptyList(), runOnce = false, onceIn = null)
    }

    override fun onCleared() {
        session?.close()
    }
}
