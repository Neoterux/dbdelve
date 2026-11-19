package dev.dbdelve.android.ui

import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.data.Profile
import dev.dbdelve.android.data.SharedConnection
import java.io.File
import kotlinx.coroutines.launch
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.SslMode
import uniffi.dbdelve_core.shortestPassphrase

/** Lends a file from the cache to the app chosen in Android's share sheet. */
fun shareFile(context: Context, name: String, bytes: ByteArray, mime: String) {
    val directory = File(context.cacheDir, "exports").apply { mkdirs() }
    val file = File(directory, name).apply { writeBytes(bytes) }
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
    val send = Intent(Intent.ACTION_SEND)
        .setType(mime)
        .putExtra(Intent.EXTRA_STREAM, uri)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    context.startActivity(Intent.createChooser(send, "Share $name").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
}

@Composable
private fun PassphraseField(label: String, value: String, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        visualTransformation = PasswordVisualTransformation(),
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false),
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
fun ExportDialog(model: AppViewModel, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val chosen = remember { mutableStateListOf<String>().apply { addAll(model.profiles.map { it.id }) } }
    var withPasswords by remember { mutableStateOf(true) }
    var passphrase by remember { mutableStateOf("") }
    var again by remember { mutableStateOf("") }
    var working by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val shortest = remember { shortestPassphrase().toInt() }
    val ready = chosen.isNotEmpty() && passphrase.length >= shortest && passphrase == again && !working
    AlertDialog(
        onDismissRequest = { if (!working) onDismiss() },
        title = { Text("Share connections") },
        text = {
            Column(Modifier.heightIn(max = 560.dp).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                model.profiles.forEach { profile ->
                    Row(
                        Modifier.fillMaxWidth().clickable { if (profile.id in chosen) chosen.remove(profile.id) else chosen.add(profile.id) },
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Checkbox(checked = profile.id in chosen, onCheckedChange = null)
                        Column(Modifier.padding(start = 8.dp)) {
                            Text(profile.name)
                            Text("${profile.host}:${profile.port}", fontFamily = Mono, fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                    }
                }
                Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 8.dp)) {
                    Text("Include passwords", modifier = Modifier.weight(1f))
                    Switch(checked = withPasswords, onCheckedChange = { withPasswords = it })
                }
                PassphraseField("Passphrase (at least $shortest characters)", passphrase) { passphrase = it }
                PassphraseField("Passphrase again", again) { again = it }
                Text(
                    "The file is encrypted with this passphrase (AES-256-GCM). Send the passphrase another way than the file.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (again.isNotEmpty() && passphrase != again) Text("The passphrases differ.", color = LocalPalette.current.red, fontSize = 12.sp)
                error?.let { Text(it, color = LocalPalette.current.red, fontSize = 12.sp) }
            }
        },
        confirmButton = {
            TextButton(enabled = ready, onClick = {
                working = true
                error = null
                scope.launch {
                    val profiles = model.profiles.filter { it.id in chosen }
                    model.exportConnections(profiles, withPasswords, passphrase)
                        .onSuccess { bytes ->
                            runCatching { shareFile(context, "connections.dbdelve", bytes, "application/octet-stream") }
                                .onSuccess { onDismiss() }
                                .onFailure { error = it.message }
                        }
                        .onFailure { error = it.message }
                    working = false
                }
            }) {
                if (working) CircularProgressIndicator(Modifier.padding(end = 8.dp).heightIn(max = 16.dp), strokeWidth = 2.dp)
                Text("Share")
            }
        },
        dismissButton = { TextButton(enabled = !working, onClick = onDismiss) { Text("Cancel") } },
    )
}

/** One connection in the import review: everything it will be saved as. */
private class Choice(shared: SharedConnection, val clashes: Boolean) {
    val originalPassword = shared.password
    var include by mutableStateOf(true)
    var name by mutableStateOf(shared.profile.name)
    var project by mutableStateOf(shared.profile.project)
    var host by mutableStateOf(shared.profile.host)
    var port by mutableStateOf(shared.profile.port.toString())
    var user by mutableStateOf(shared.profile.user)
    var database by mutableStateOf(shared.profile.database)
    var ssl by mutableStateOf(shared.profile.sslMode)
    // Imported means new on this phone, and a new connection starts
    // Read-only: the sender's mode is theirs to have chosen, not yours.
    var mode by mutableStateOf(Mode.READ_ONLY)
    val sentMode = shared.profile.mode
    var withPassword by mutableStateOf(shared.password != null)
    var clash by mutableStateOf(AppViewModel.Clash.KEEP_BOTH)
    val engine = shared.profile.engine
    val id = shared.profile.id

    fun portNumber() = port.toIntOrNull()?.takeIf { it in 1..65535 }

    fun valid() = name.isNotBlank() && host.isNotBlank() && portNumber() != null

    fun toShared() = SharedConnection(
        Profile(id, name.trim(), project.trim(), engine, host.trim(), portNumber()!!, user, database.trim(), ssl, mode),
        if (withPassword) originalPassword else null,
    )
}

/**
 * Opening a shared file: the passphrase first, then every connection it holds,
 * each editable and each with its own answer to a name already here.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ImportFlow(model: AppViewModel, uri: Uri, onDismiss: () -> Unit, onImported: (Int) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var passphrase by remember { mutableStateOf("") }
    var working by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var choices by remember { mutableStateOf<List<Choice>?>(null) }

    val opened = choices
    if (opened == null) {
        AlertDialog(
            onDismissRequest = { if (!working) onDismiss() },
            title = { Text("Import connections") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    PassphraseField("Passphrase", passphrase) { passphrase = it }
                    error?.let { Text(it, color = LocalPalette.current.red, fontSize = 12.sp) }
                }
            },
            confirmButton = {
                TextButton(enabled = passphrase.isNotEmpty() && !working, onClick = {
                    working = true
                    error = null
                    scope.launch {
                        val bytes = runCatching { context.contentResolver.openInputStream(uri)!!.use { it.readBytes() } }
                        bytes.onFailure { error = "Could not read the file." }
                        bytes.getOrNull()?.let { file ->
                            model.openConnectionsFile(file, passphrase)
                                .onSuccess { shared -> choices = shared.map { Choice(it, model.profiles.any { here -> here.name == it.profile.name }) } }
                                .onFailure { error = it.message }
                        }
                        working = false
                    }
                }) { Text(if (working) "Opening…" else "Open") }
            },
            dismissButton = { TextButton(enabled = !working, onClick = onDismiss) { Text("Cancel") } },
        )
        return
    }

    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        SheetTitle("Review ${opened.size} connection${if (opened.size == 1) "" else "s"}", "Change anything before it is saved")
        LazyColumn(Modifier.weight(1f, fill = false).padding(horizontal = 12.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            itemsIndexed(opened) { _, choice -> ChoiceCard(choice) }
        }
        val included = opened.filter { it.include }
        val ready = included.isNotEmpty() && included.all { it.valid() }
        Row(Modifier.fillMaxWidth().navigationBarsPadding().padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(
                if (included.all { it.valid() }) "${included.size} to import" else "Fix the highlighted fields",
                modifier = Modifier.weight(1f),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            TextButton(onClick = onDismiss) { Text("Cancel") }
            Button(enabled = ready, onClick = {
                onImported(model.importConnections(included.map { it.toShared() to it.clash }))
            }) { Text("Import") }
        }
    }
}

@Composable
private fun ChoiceCard(choice: Choice) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(10.dp)) {
        Column(Modifier.fillMaxWidth().padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(checked = choice.include, onCheckedChange = { choice.include = it })
                Text("${choice.engine.badge()}  ${choice.name}", fontWeight = FontWeight.SemiBold, modifier = Modifier.weight(1f))
            }
            if (!choice.include) return@Column
            Field("Name", choice.name) { choice.name = it }
            Field("Project", choice.project) { choice.project = it }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Field("Host", choice.host, Modifier.weight(1f)) { choice.host = it }
                Field("Port", choice.port, Modifier.weight(0.5f), error = choice.portNumber() == null) { choice.port = it }
            }
            Field("User", choice.user) { choice.user = it }
            Field("Database", choice.database) { choice.database = it }
            Choose("TLS", listOf(SslMode.VERIFY_FULL to "Verified", SslMode.DISABLE to "Off"), choice.ssl) { choice.ssl = it }
            Choose("Mode", Mode.entries.map { it to it.label() }, choice.mode) { choice.mode = it }
            if (choice.sentMode != Mode.READ_ONLY) {
                Hint("It was ${choice.sentMode.label()} where it came from; here it starts Read-only unless you choose.")
            }
            if (choice.originalPassword != null) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Save its password", modifier = Modifier.weight(1f))
                    Switch(checked = choice.withPassword, onCheckedChange = { choice.withPassword = it })
                }
            } else {
                Hint("No password in the file: add one by editing the connection after importing.")
            }
            if (choice.clashes) {
                Choose(
                    "A connection named ${choice.name} is already here",
                    listOf(AppViewModel.Clash.KEEP_BOTH to "Keep both", AppViewModel.Clash.REPLACE to "Replace", AppViewModel.Clash.SKIP to "Skip"),
                    choice.clash,
                ) { choice.clash = it }
            }
        }
    }
}

@Composable
private fun Field(label: String, value: String, modifier: Modifier = Modifier.fillMaxWidth(), error: Boolean = false, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        isError = error || (label != "Project" && label != "User" && label != "Database" && value.isBlank()),
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
        modifier = modifier,
    )
}

@Composable
private fun <T> Choose(label: String, options: List<Pair<T, String>>, selected: T, onSelect: (T) -> Unit) {
    Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
        options.forEachIndexed { at, (value, text) ->
            SegmentedButton(
                selected = selected == value,
                onClick = { onSelect(value) },
                shape = SegmentedButtonDefaults.itemShape(at, options.size),
            ) { Text(text, fontSize = 12.sp) }
        }
    }
}

@Composable
private fun Hint(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}
