package dev.dbdelve.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.Screen
import dev.dbdelve.android.data.Profile
import java.util.UUID
import uniffi.dbdelve_core.Engine
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.SslMode
import uniffi.dbdelve_core.defaultPort

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectionForm(model: AppViewModel, editing: Profile?, duplicate: Boolean) {
    // A duplicate keeps the mode of the one it copies; an edit never changes
    // one; a new connection starts Read-only.
    val creating = editing == null || duplicate
    var engine by rememberSaveable { mutableStateOf(editing?.engine ?: Engine.POSTGRES) }
    var name by rememberSaveable { mutableStateOf(editing?.name?.let { if (duplicate) "$it copy" else it } ?: "") }
    var project by rememberSaveable { mutableStateOf(editing?.project ?: "") }
    var host by rememberSaveable { mutableStateOf(editing?.host ?: "") }
    var port by rememberSaveable { mutableStateOf((editing?.port ?: defaultPort(engine).toInt()).toString()) }
    var user by rememberSaveable { mutableStateOf(editing?.user ?: "") }
    // Not saveable, on purpose: saved state is a Bundle the system may write
    // to disk, and a password does not belong there. Rotating re-reads it.
    var password by remember { mutableStateOf(editing?.let { model.password(it) } ?: "") }
    var showPassword by remember { mutableStateOf(false) }
    var database by rememberSaveable { mutableStateOf(editing?.database ?: "") }
    var ssl by rememberSaveable { mutableStateOf(editing?.sslMode ?: SslMode.VERIFY_FULL) }
    var mode by rememberSaveable { mutableStateOf(editing?.mode ?: Mode.READ_ONLY) }

    val portNumber = port.toIntOrNull()?.takeIf { it in 1..65535 }
    val databaseNeeded = engine == Engine.MONGO && database.isBlank()
    val valid = name.isNotBlank() && host.isNotBlank() && portNumber != null && !databaseNeeded

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(if (creating) "New connection" else "Edit ${editing?.name}") },
                navigationIcon = {
                    IconButton(onClick = { model.screen = Screen.Connections }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
                actions = {
                    TextButton(
                        enabled = valid,
                        onClick = {
                            model.saveProfile(
                                Profile(
                                    id = if (creating) UUID.randomUUID().toString() else editing!!.id,
                                    name = name.trim(),
                                    project = project.trim(),
                                    engine = engine,
                                    host = host.trim(),
                                    port = portNumber!!,
                                    user = user,
                                    database = database.trim(),
                                    sslMode = ssl,
                                    mode = mode,
                                ),
                                password,
                            )
                        },
                    ) { Text("Save") }
                },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .padding(padding)
                .fillMaxSize()
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Label("Engine")
            SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                Engine.entries.forEachIndexed { at, choice ->
                    SegmentedButton(
                        selected = engine == choice,
                        onClick = {
                            if (port == defaultPort(engine).toString()) port = defaultPort(choice).toString()
                            engine = choice
                        },
                        shape = SegmentedButtonDefaults.itemShape(at, Engine.entries.size),
                    ) { Text(choice.label()) }
                }
            }
            if (engine == Engine.MY_SQL) {
                Hint("MySQL 5.6.5 or later, which can hold a session to reads.")
            }
            Field("Name", name) { name = it }
            Field("Project", project, placeholder = "Groups connections in the list") { project = it }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Field("Host", host, Modifier.weight(1f), keyboard = KeyboardType.Uri) { host = it }
                Field("Port", port, Modifier.weight(0.45f), keyboard = KeyboardType.Number) { port = it }
            }
            Field("User", user, keyboard = KeyboardType.Email) { user = it }
            OutlinedTextField(
                value = password,
                onValueChange = { password = it },
                label = { Text("Password") },
                singleLine = true,
                visualTransformation = if (showPassword) VisualTransformation.None else PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                trailingIcon = { TextButton(onClick = { showPassword = !showPassword }) { Text(if (showPassword) "Hide" else "Show") } },
                modifier = Modifier.fillMaxWidth(),
            )
            Field(
                "Database",
                database,
                placeholder = if (engine == Engine.MONGO) "Required; also where the user is defined" else "Optional",
            ) { database = it }

            Label("TLS")
            SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                listOf(SslMode.VERIFY_FULL to "Verified", SslMode.DISABLE to "Off").forEachIndexed { at, (choice, text) ->
                    SegmentedButton(
                        selected = ssl == choice,
                        onClick = { ssl = choice },
                        shape = SegmentedButtonDefaults.itemShape(at, 2),
                    ) { Text(text) }
                }
            }
            Hint(
                when (ssl) {
                    SslMode.VERIFY_FULL ->
                        "TLS 1.2 or later, certificate and host name checked. If the server cannot, the connection fails and says why; it never falls back to plaintext."
                    SslMode.DISABLE -> "Password and rows cross the network unencrypted."
                },
            )

            if (creating) {
                Label("Mode")
                SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                    Mode.entries.forEachIndexed { at, choice ->
                        SegmentedButton(
                            selected = mode == choice,
                            onClick = { mode = choice },
                            shape = SegmentedButtonDefaults.itemShape(at, Mode.entries.size),
                        ) { Text(choice.label()) }
                    }
                }
            }
            if (!valid) {
                Hint(if (databaseNeeded) "MongoDB needs a database." else "Name, host and a port from 1 to 65535 are required.")
            }
        }
    }
}

@Composable
private fun Field(
    label: String,
    value: String,
    modifier: Modifier = Modifier.fillMaxWidth(),
    placeholder: String? = null,
    keyboard: KeyboardType = KeyboardType.Text,
    onChange: (String) -> Unit,
) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        placeholder = if (placeholder != null) { { Text(placeholder) } } else null,
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = keyboard, autoCorrectEnabled = false),
        modifier = modifier,
    )
}

@Composable
private fun Label(text: String) {
    Text(text, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun Hint(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}
