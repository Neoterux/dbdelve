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
import dev.dbdelve.android.data.ConnectionBundle
import dev.dbdelve.android.data.SavedQuery
import dev.dbdelve.android.data.SavedQueryStore
import dev.dbdelve.android.data.SharedConnection
import dev.dbdelve.android.data.TabStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.dbdelve_core.Account
import uniffi.dbdelve_core.Binding
import uniffi.dbdelve_core.CatalogEntry
import uniffi.dbdelve_core.CoreException
import uniffi.dbdelve_core.Destructive
import uniffi.dbdelve_core.DbUser
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.Prepared
import uniffi.dbdelve_core.ResultSet
import uniffi.dbdelve_core.RunOutcome
import uniffi.dbdelve_core.Session
import uniffi.dbdelve_core.Stop
import uniffi.dbdelve_core.UserAction
import uniffi.dbdelve_core.Verdict
import uniffi.dbdelve_core.fillParameters
import uniffi.dbdelve_core.openConnections
import uniffi.dbdelve_core.sealConnections
import uniffi.dbdelve_core.statementAt

class QueryTab(val id: Long, title: String, text: String) {
    var title by mutableStateOf(title)
    var text by mutableStateOf(TextFieldValue(text, TextRange(text.length)))
    var result by mutableStateOf<ResultSet?>(null)
    var error by mutableStateOf<String?>(null)
    /** What was sent, exactly, so the results say what produced them. */
    var ran by mutableStateOf<String?>(null)
}

/**
 * A statement the gate stopped, held while the user answers. Nothing was
 * sent. [retry] sends it again with the answer, through the same gate.
 */
data class PendingGate(
    val statement: String,
    val stop: Stop,
    val verdict: Verdict,
    val confirmed: List<Destructive>,
    val onceIn: Mode?,
    val retry: (confirmed: List<Destructive>, runOnce: Boolean, onceIn: Mode?) -> Unit,
)

/** A user-management statement built and waiting for the user to read it. */
class Review(val title: String, val prepared: Prepared)

sealed interface Screen {
    data object Connections : Screen
    data class Edit(val profile: Profile?, val duplicate: Boolean = false) : Screen
    data object Workspace : Screen
}

enum class Pane { QUERY, RESULTS, SCHEMA, USERS }

fun Throwable.reason(): String = when (this) {
    is CoreException.Connect -> reason
    is CoreException.Query -> reason
    else -> message ?: toString()
}

class AppViewModel(
    private val store: ProfileStore,
    private val secrets: Secrets,
    private val tabStore: TabStore,
    private val savedStore: SavedQueryStore,
    /** Called with the connection's name while a statement runs, null after. */
    private val keepAlive: (connection: String?) -> Unit,
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
        private set
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

    /**
     * The saved password, or blank when the Keystore can no longer open it
     * (a key invalidated by a lock-screen reset): the form then asks again
     * rather than the app crashing.
     */
    fun password(profile: Profile): String = runCatching { secrets.password(profile.id) }.getOrDefault("")

    fun deleteProfile(profile: Profile) {
        profiles = profiles.filterNot { it.id == profile.id }
        store.save(profiles)
        secrets.forget(profile.id)
        tabStore.forget(profile.id)
        allSaved = allSaved.filterNot { it.profileId == profile.id }
        savedStore.save(allSaved)
    }

    /**
     * The chosen connections sealed into a file under [passphrase]; with their
     * passwords only when asked. Slow on purpose (the key derivation), so off
     * the main thread.
     */
    suspend fun exportConnections(chosen: List<Profile>, withPasswords: Boolean, passphrase: String): Result<ByteArray> =
        withContext(Dispatchers.IO) {
            runCatching {
                val shared = chosen.map { SharedConnection(it, if (withPasswords) password(it) else null) }
                sealConnections(ConnectionBundle.toJson(shared), passphrase)
            }
        }

    /** Opens a shared file: a wrong passphrase or a changed byte fails here. */
    suspend fun openConnectionsFile(file: ByteArray, passphrase: String): Result<List<SharedConnection>> =
        withContext(Dispatchers.IO) {
            runCatching { ConnectionBundle.fromJson(openConnections(file, passphrase)) }
                .recoverCatching { throw IllegalStateException(it.reason()) }
        }

    enum class Clash { KEEP_BOTH, REPLACE, SKIP }

    /**
     * Saves what the import dialog settled on. A name already here is kept
     * beside it, replaced in place (its id, tabs and saved queries kept), or
     * skipped, as chosen per connection.
     */
    fun importConnections(chosen: List<Pair<SharedConnection, Clash>>): Int {
        var imported = 0
        var next = profiles
        chosen.forEach { (shared, clash) ->
            val existing = next.find { it.name == shared.profile.name }
            val profile = when {
                existing == null -> shared.profile
                clash == Clash.SKIP -> return@forEach
                clash == Clash.REPLACE -> shared.profile.copy(id = existing.id)
                else -> shared.profile.copy(name = uniqueName(shared.profile.name, next))
            }
            shared.password?.let { secrets.setPassword(profile.id, it) }
            next = next.filterNot { it.id == profile.id } + profile
            imported++
        }
        profiles = next.sortedWith(compareBy({ it.project }, { it.name }))
        store.save(profiles)
        return imported
    }

    private fun uniqueName(name: String, taken: List<Profile>): String =
        generateSequence(2) { it + 1 }.map { "$name ($it)" }.first { candidate -> taken.none { it.name == candidate } }

    private var allSaved by mutableStateOf(savedStore.load())

    /** The open connection's saved queries, by name. */
    val saved: List<SavedQuery>
        get() = profile?.let { open -> allSaved.filter { it.profileId == open.id }.sortedBy { it.name.lowercase() } }.orEmpty()

    /** Saves [text] as [name] for this connection, replacing one of that name. */
    fun saveQuery(name: String, text: String) {
        val profileId = profile?.id ?: return
        val kept = allSaved.filterNot { it.profileId == profileId && it.name == name }
        allSaved = kept + SavedQuery(SavedQueryStore.newId(), profileId, name, text)
        savedStore.save(allSaved)
        tab?.title = name
        saveTabs()
    }

    fun deleteSaved(query: SavedQuery) {
        allSaved = allSaved.filterNot { it.id == query.id }
        savedStore.save(allSaved)
    }

    /** Opens a saved query in a tab of its own, as saved, to edit or run. */
    fun openSaved(query: SavedQuery) {
        newTab(query.name, query.text)
        pane = Pane.QUERY
    }

    /**
     * Fills a saved query's parameters and runs it, in a tab that shows the
     * filled statement: what ran is on screen, and runs through the gate like
     * anything typed. Returns why it could not be filled, or null.
     */
    fun runSaved(query: SavedQuery, bindings: List<Binding>): String? {
        val session = session ?: return "Not connected."
        val filled = runCatching { fillParameters(session.engine(), query.text, bindings) }
            .getOrElse { return it.reason() }
        val tab = newTab(query.name, filled)
        execute(tab, filled, emptyList(), runOnce = false, onceIn = null)
        return null
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
                restoreTabs(profile)
                pane = Pane.QUERY
                screen = Screen.Workspace
                refreshCatalog()
            }.onFailure { connectError = "${profile.name}: ${it.reason()}" }
        }
    }

    fun disconnect() {
        saveTabs(now = true)
        val closing = session
        session = null
        running = false
        profile = null
        catalog = emptyList()
        gate = null
        users = emptyList()
        grants = emptyMap()
        review = null
        userMessage = null
        screen = Screen.Connections
        viewModelScope.launch(Dispatchers.IO) { closing?.close() }
    }

    fun newTab(title: String = "Query ${nextTabId + 1}", text: String = ""): QueryTab {
        val tab = QueryTab(nextTabId++, title, text)
        tabs.add(tab)
        current = tabs.lastIndex
        saveTabs()
        return tab
    }

    fun closeTab(tab: QueryTab) {
        tabs.remove(tab)
        if (tabs.isEmpty()) newTab()
        current = current.coerceIn(0, tabs.lastIndex)
        saveTabs()
    }

    fun select(at: Int) {
        current = at
        saveTabs()
    }

    /** Every keystroke and key-row press goes through here, so none is unsaved. */
    fun edit(tab: QueryTab, value: TextFieldValue) {
        val changed = tab.text.text != value.text
        tab.text = value
        if (changed) saveTabs()
    }

    private fun restoreTabs(profile: Profile) {
        tabs.clear()
        nextTabId = 0
        val (saved, at) = tabStore.load(profile.id)
        if (saved.isEmpty()) {
            newTab()
            return
        }
        saved.forEach { tabs.add(QueryTab(nextTabId++, it.title, it.text)) }
        current = at.coerceIn(0, tabs.lastIndex)
    }

    private var saving: Job? = null

    /**
     * Writes the tabs a moment after the last change, so typing is not a
     * write per keystroke; [now] writes at once, for leaving the connection.
     */
    private fun saveTabs(now: Boolean = false) {
        val id = profile?.id ?: return
        val snapshot = tabs.map { TabStore.Saved(it.title, it.text.text) }
        val at = current
        saving?.cancel()
        saving = viewModelScope.launch {
            if (!now) delay(600)
            withContext(Dispatchers.IO) { tabStore.save(id, snapshot, at) }
        }
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
        val limit = rowLimit.toUInt()
        send(
            shown = statement,
            confirmed = confirmed,
            onceIn = onceIn,
            retry = { again, once, mode -> execute(tab, statement, again, once, mode) },
            call = { it.run(statement, limit, confirmed, runOnce, onceIn) },
            done = { result ->
                tab.result = result
                tab.error = null
                tab.ran = statement
                pane = Pane.RESULTS
            },
            failed = { reason ->
                tab.error = reason
                tab.ran = statement
                pane = Pane.RESULTS
            },
        )
    }

    /** Sends one statement through the gate; a stop becomes [gate]. */
    private fun send(
        shown: String,
        confirmed: List<Destructive>,
        onceIn: Mode?,
        retry: (List<Destructive>, Boolean, Mode?) -> Unit,
        call: (Session) -> RunOutcome,
        done: (ResultSet) -> Unit,
        failed: (String) -> Unit,
    ) {
        val session = session ?: return
        if (running) return
        running = true
        keepAlive(profile?.name)
        // ponytail: the service keeps the process alive, but the session lives
        // in the view model, so a statement still ends if Android destroys the
        // activity outright. A service that owns the session is the upgrade path.
        viewModelScope.launch {
            val outcome = try {
                withContext(Dispatchers.IO) { runCatching { call(session) } }
            } finally {
                // Also when the view model is cleared mid-statement (the app
                // swiped away): otherwise the foreground service outlives it.
                keepAlive(null)
                if (this@AppViewModel.session === session) running = false
            }
            // A statement that ends after a disconnect, or after switching to
            // another connection, must not land in the new one's state.
            if (this@AppViewModel.session !== session) return@launch
            outcome.onSuccess { outcome ->
                when (outcome) {
                    is RunOutcome.Done -> done(outcome.result)
                    is RunOutcome.Stopped ->
                        gate = PendingGate(shown, outcome.stop, outcome.verdict, confirmed, onceIn, retry)
                }
            }.onFailure { failed(it.reason()) }
        }
    }

    fun dismissGate() {
        gate = null
    }

    /** Runs the stopped statement once in [mode]; the connection's own mode stays. */
    fun runOnceIn(mode: Mode) {
        val pending = gate ?: return
        gate = null
        pending.retry(pending.confirmed, false, mode)
    }

    fun confirm(kind: Destructive) {
        val pending = gate ?: return
        gate = null
        pending.retry(pending.confirmed + kind, false, pending.onceIn)
    }

    fun runUnreadableOnce() {
        val pending = gate ?: return
        gate = null
        pending.retry(pending.confirmed, true, pending.onceIn)
    }

    /** Switches the connection, then runs what was stopped. */
    fun switchAndRun(mode: Mode) {
        val pending = gate ?: return
        gate = null
        setMode(mode) { pending.retry(pending.confirmed, false, null) }
    }

    var users by mutableStateOf<List<DbUser>>(emptyList())
        private set
    var usersError by mutableStateOf<String?>(null)
        private set
    var grants by mutableStateOf<Map<Account, List<String>>>(emptyMap())
        private set
    var review by mutableStateOf<Review?>(null)
        private set
    /** What the last user-management statement did, or why it failed. */
    var userMessage by mutableStateOf<String?>(null)

    var refreshingUsers by mutableStateOf(false)
        private set

    fun refreshUsers() {
        val session = session ?: return
        refreshingUsers = true
        viewModelScope.launch {
            val listed = withContext(Dispatchers.IO) { runCatching { session.users() } }
            refreshingUsers = false
            listed.onSuccess {
                users = it
                usersError = null
            }.onFailure { usersError = it.reason() }
        }
    }

    fun loadGrants(account: Account) {
        val session = session ?: return
        viewModelScope.launch {
            val read = withContext(Dispatchers.IO) { runCatching { session.userGrants(account) } }
            grants = grants + (account to read.getOrElse { listOf(it.reason()) })
        }
    }

    /** Builds the statement for [action] and holds it for the user to read. */
    fun prepare(title: String, action: UserAction) {
        val session = session ?: return
        viewModelScope.launch {
            val built = withContext(Dispatchers.IO) { runCatching { session.prepareUser(action) } }
            built.onSuccess { review = Review(title, it) }
                .onFailure { userMessage = it.reason() }
        }
    }

    fun dismissReview() {
        review?.prepared?.close()
        review = null
    }

    /**
     * Runs the reviewed statement. The review named any destructive kind, so
     * reading it is the confirmation; a mode it needs still goes through the
     * gate's own dialog.
     */
    fun runReviewed() {
        val reviewed = review ?: return
        review = null
        runPrepared(reviewed, reviewed.prepared.verdict().destructive, null)
    }

    private fun runPrepared(reviewed: Review, confirmed: List<Destructive>, onceIn: Mode?) {
        send(
            shown = reviewed.prepared.shown(),
            confirmed = confirmed,
            onceIn = onceIn,
            retry = { again, _, mode -> runPrepared(reviewed, again, mode) },
            call = { it.runPrepared(reviewed.prepared, confirmed, onceIn) },
            done = {
                userMessage = "${reviewed.title}: done."
                refreshUsers()
                grants.keys.forEach(::loadGrants)
            },
            failed = { userMessage = "${reviewed.title}: $it" },
        )
    }

    /** The only place a connection's mode changes, and it is saved with it. */
    fun setMode(next: Mode, then: () -> Unit = {}) {
        val session = session ?: return
        val profile = profile ?: return
        viewModelScope.launch {
            val changed = withContext(Dispatchers.IO) { runCatching { session.setMode(next) } }
            // It may complete after the user left this connection: the
            // profile still takes the mode, the screen of another does not.
            val current = this@AppViewModel.session === session
            changed.onSuccess {
                val saved = profile.copy(mode = next)
                profiles = profiles.map { if (it.id == saved.id) saved else it }
                store.save(profiles)
                if (!current) return@onSuccess
                mode = next
                this@AppViewModel.profile = saved
                then()
            }.onFailure { failure -> if (current) tab?.let { it.error = failure.reason() } }
        }
    }

    fun cancel() {
        val session = session ?: return
        // A thread of its own: the session's lock is held by what it stops.
        // A disconnect may destroy the session first; a cancel then has
        // nothing left to stop, and must not take the app down trying.
        Thread { runCatching { session.cancel() } }.start()
    }

    var refreshingCatalog by mutableStateOf(false)
        private set

    fun refreshCatalog() {
        val session = session ?: return
        refreshingCatalog = true
        viewModelScope.launch {
            val listed = withContext(Dispatchers.IO) { runCatching { session.catalog() } }
            refreshingCatalog = false
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
