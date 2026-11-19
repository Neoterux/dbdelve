package dev.dbdelve.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.AddModerator
import androidx.compose.material.icons.filled.Block
import androidx.compose.material.icons.filled.GroupAdd
import androidx.compose.material.icons.filled.Key
import androidx.compose.material.icons.filled.PersonRemove
import androidx.compose.material.icons.filled.RemoveCircleOutline
import androidx.compose.material.icons.filled.RemoveModerator
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material.icons.automirrored.filled.Login
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.Review
import uniffi.dbdelve_core.Account
import uniffi.dbdelve_core.DbUser
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.RoleRef
import uniffi.dbdelve_core.Scope
import uniffi.dbdelve_core.ScopeKind
import uniffi.dbdelve_core.UserAbilities
import uniffi.dbdelve_core.UserAction
import uniffi.dbdelve_core.userAbilities

/** Which form is open over the user list. */
private sealed interface Form {
    data object Create : Form
    data class Detail(val user: DbUser) : Form
    data class Password(val account: Account) : Form
    data class Privileges(val account: Account, val grant: Boolean) : Form
    data class Role(val account: Account) : Form
}

private fun Account.label() = if (host.isEmpty()) name else "$name@$host"

private fun RoleRef.label() = if (database.isEmpty()) role else "$role@$database"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun UsersPane(model: AppViewModel) {
    val session = model.session ?: return
    val abilities = remember(session) { userAbilities(session.engine()) }
    val palette = LocalPalette.current
    var form by remember { mutableStateOf<Form?>(null) }
    LaunchedEffect(session) { model.refreshUsers() }

    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("Users", style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            TextButton(onClick = { form = Form.Create }) {
                Icon(Icons.Default.Add, contentDescription = null)
                Text("New user", modifier = Modifier.padding(start = 4.dp))
            }
        }
        if (model.mode != Mode.FULL) {
            Text(
                "Changes need Full. ${model.profile?.name} is ${model.mode.label()}: each change asks first.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 2.dp),
            )
        }
        model.userMessage?.let { message ->
            Row(
                Modifier.fillMaxWidth().padding(12.dp).background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(8.dp)).padding(start = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(message, fontFamily = Mono, fontSize = 12.sp, modifier = Modifier.weight(1f).padding(vertical = 8.dp))
                TextButton(onClick = { model.userMessage = null }) { Text("Dismiss") }
            }
        }
        model.usersError?.let {
            Text(it, color = palette.red, fontFamily = Mono, fontSize = 12.sp, modifier = Modifier.padding(16.dp))
        }
        PullToRefreshBox(isRefreshing = model.refreshingUsers, onRefresh = { model.refreshUsers() }, modifier = Modifier.fillMaxSize()) {
            LazyColumn(Modifier.fillMaxSize()) {
                items(model.users, key = { it.account.name to it.account.host }) { user ->
                    UserRow(user, abilities) { form = Form.Detail(user) }
                    HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
                }
            }
        }
    }

    when (val open = form) {
        null -> {}
        Form.Create -> CreateForm(abilities, onDismiss = { form = null }) { action ->
            form = null
            model.prepare("Create ${action.account.label()}", action)
        }
        is Form.Detail -> UserDetail(model, open.user, abilities, onDismiss = { form = null }) { form = it }
        is Form.Password -> PasswordForm(open.account, abilities, onDismiss = { form = null }) { action ->
            form = null
            model.prepare("Change ${open.account.label()}'s password", action)
        }
        is Form.Privileges -> PrivilegesForm(model, open.account, open.grant, abilities, onDismiss = { form = null }) { action ->
            form = null
            model.prepare("${if (open.grant) "Grant to" else "Revoke from"} ${open.account.label()}", action)
        }
        is Form.Role -> RoleForm(open.account, abilities, onDismiss = { form = null }) { action ->
            form = null
            model.prepare("Grant ${action.role} to ${open.account.label()}", action)
        }
    }
    model.review?.let { ReviewDialog(model, it) }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun UserRow(user: DbUser, abilities: UserAbilities, open: () -> Unit) {
    val palette = LocalPalette.current
    Column(Modifier.fillMaxWidth().clickable(onClick = open).padding(horizontal = 16.dp, vertical = 10.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(user.account.name, fontWeight = FontWeight.SemiBold)
            if (abilities.host) Text("@${user.account.host}", fontFamily = Mono, fontSize = 12.sp, color = palette.dim)
            if (user.superuser) Tag("superuser", palette.red)
            if (!user.canLogin) Tag("no login", palette.faint)
        }
        val details = user.attributes + user.roles.map { "member of ${it.label()}" }
        if (details.isNotEmpty()) {
            FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.padding(top = 4.dp)) {
                details.forEach { Text(it, fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            }
        }
    }
}

@Composable
private fun Tag(text: String, color: androidx.compose.ui.graphics.Color) {
    Text(
        text,
        fontSize = 10.sp,
        fontWeight = FontWeight.Bold,
        color = color,
        modifier = Modifier.background(color.copy(alpha = 0.14f), RoundedCornerShape(5.dp)).padding(horizontal = 6.dp, vertical = 2.dp),
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun UserDetail(
    model: AppViewModel,
    user: DbUser,
    abilities: UserAbilities,
    onDismiss: () -> Unit,
    open: (Form) -> Unit,
) {
    val palette = LocalPalette.current
    val account = user.account
    LaunchedEffect(account) { model.loadGrants(account) }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        SheetTitle(account.label())
            Column(
                Modifier.heightIn(max = 560.dp).verticalScroll(rememberScrollState()).navigationBarsPadding().padding(horizontal = 24.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Text("What it may do", style = MaterialTheme.typography.labelLarge)
                val grants = model.grants[account]
                when {
                    grants == null -> Text("Reading…", color = palette.faint)
                    grants.isEmpty() -> Text("No table privileges of its own.", color = palette.faint)
                    else -> grants.forEach { Text(it, fontFamily = Mono, fontSize = 12.sp) }
                }
                if (abilities.roles && user.roles.isNotEmpty()) {
                    Text("Roles", style = MaterialTheme.typography.labelLarge, modifier = Modifier.padding(top = 6.dp))
                    user.roles.forEach { role ->
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(role.label(), fontFamily = Mono, fontSize = 12.sp, modifier = Modifier.weight(1f))
                            IconButton(onClick = {
                                onDismiss()
                                model.prepare(
                                    "Revoke ${role.label()} from ${account.label()}",
                                    UserAction.RevokeRole(account, role.role, role.database),
                                )
                            }) { Icon(Icons.Default.RemoveCircleOutline, contentDescription = "Revoke ${role.label()}", tint = palette.red) }
                        }
                    }
                }
                HorizontalDivider(Modifier.padding(vertical = 6.dp))
                Action("Change password", Icons.Default.Key) { open(Form.Password(account)) }
                if (abilities.privileges.isNotEmpty()) {
                    Action("Grant privileges", Icons.Default.AddModerator) { open(Form.Privileges(account, grant = true)) }
                    Action("Revoke privileges", Icons.Default.RemoveModerator) { open(Form.Privileges(account, grant = false)) }
                }
                if (abilities.roles) Action("Grant a role", Icons.Default.GroupAdd) { open(Form.Role(account)) }
                if (abilities.loginToggle) {
                    Action(if (user.canLogin) "Disable login" else "Enable login", if (user.canLogin) Icons.Default.Block else Icons.AutoMirrored.Filled.Login) {
                        onDismiss()
                        model.prepare(
                            "${if (user.canLogin) "Disable" else "Enable"} login for ${account.label()}",
                            UserAction.SetLogin(account, !user.canLogin),
                        )
                    }
                }
                Action("Drop ${account.label()}", Icons.Default.PersonRemove, palette.red) {
                    onDismiss()
                    model.prepare("Drop ${account.label()}", UserAction.Drop(account))
                }
            }
    }
}

@Composable
private fun Action(
    text: String,
    icon: ImageVector,
    color: androidx.compose.ui.graphics.Color = MaterialTheme.colorScheme.primary,
    onClick: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Icon(icon, null, tint = color, modifier = Modifier.size(20.dp))
        Text(text, color = color, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun PasswordField(password: String, note: String, onChange: (String) -> Unit) {
    var visible by remember { mutableStateOf(false) }
    OutlinedTextField(
        value = password,
        onValueChange = onChange,
        label = { Text("Password") },
        singleLine = true,
        visualTransformation = if (visible) VisualTransformation.None else PasswordVisualTransformation(),
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        trailingIcon = {
            IconButton(onClick = { visible = !visible }) {
                Icon(
                    if (visible) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                    contentDescription = if (visible) "Hide password" else "Show password",
                )
            }
        },
        modifier = Modifier.fillMaxWidth(),
    )
    Text(note, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun CreateForm(abilities: UserAbilities, onDismiss: () -> Unit, submit: (UserAction.Create) -> Unit) {
    var name by remember { mutableStateOf("") }
    var host by remember { mutableStateOf("%") }
    var password by remember { mutableStateOf("") }
    var canLogin by remember { mutableStateOf(true) }
    val limit = abilities.nameLimit.toInt()
    val tooLong = limit > 0 && name.length > limit
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New user") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Name") },
                    singleLine = true,
                    isError = tooLong,
                    supportingText = if (limit > 0) { { Text("${name.length} / $limit") } } else null,
                    keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
                    modifier = Modifier.fillMaxWidth(),
                )
                if (abilities.host) {
                    OutlinedTextField(
                        value = host,
                        onValueChange = { host = it },
                        label = { Text("Host it may connect from") },
                        supportingText = { Text("% is any host") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, autoCorrectEnabled = false),
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                // A blank password is a valid one, and is not warned about.
                PasswordField(password, abilities.passwordNote) { password = it }
                if (abilities.loginToggle) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Can log in", modifier = Modifier.weight(1f))
                        Switch(checked = canLogin, onCheckedChange = { canLogin = it })
                    }
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = name.isNotBlank() && !tooLong,
                onClick = { submit(UserAction.Create(Account(name.trim(), if (abilities.host) host.trim() else ""), password, canLogin)) },
            ) { Text("Review") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun PasswordForm(account: Account, abilities: UserAbilities, onDismiss: () -> Unit, submit: (UserAction) -> Unit) {
    var password by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Password for ${account.label()}") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(10.dp)) { PasswordField(password, abilities.passwordNote) { password = it } } },
        confirmButton = { TextButton(onClick = { submit(UserAction.SetPassword(account, password)) }) { Text("Review") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PrivilegesForm(
    model: AppViewModel,
    account: Account,
    grant: Boolean,
    abilities: UserAbilities,
    onDismiss: () -> Unit,
    submit: (UserAction) -> Unit,
) {
    val chosen = remember { mutableStateListOf<String>() }
    var kind by remember { mutableStateOf(abilities.scopes.first()) }
    val schemas = remember(model.catalog) { model.catalog.map { it.schema }.distinct() }
    var schema by remember { mutableStateOf(schemas.firstOrNull() ?: "") }
    var table by remember { mutableStateOf("") }
    val tables = model.catalog.filter { it.schema == schema }.map { it.name }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("${if (grant) "Grant to" else "Revoke from"} ${account.label()}") },
        text = {
            Column(Modifier.heightIn(max = 480.dp).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    abilities.privileges.forEach { privilege ->
                        FilterChip(
                            selected = privilege in chosen,
                            onClick = { if (privilege in chosen) chosen.remove(privilege) else chosen.add(privilege) },
                            label = { Text(privilege, fontSize = 12.sp) },
                        )
                    }
                }
                SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                    abilities.scopes.forEachIndexed { at, each ->
                        SegmentedButton(
                            selected = kind == each,
                            onClick = { kind = each },
                            shape = SegmentedButtonDefaults.itemShape(at, abilities.scopes.size),
                        ) {
                            Text(
                                when (each) {
                                    ScopeKind.SERVER -> "Everything"
                                    ScopeKind.SCHEMA -> "All tables"
                                    ScopeKind.TABLE -> "One table"
                                },
                                fontSize = 12.sp,
                            )
                        }
                    }
                }
                if (kind != ScopeKind.SERVER) {
                    Picker("In", schema, schemas) { schema = it; table = "" }
                }
                if (kind == ScopeKind.TABLE) {
                    Picker("Table", table, tables) { table = it }
                }
            }
        },
        confirmButton = {
            val scope = when (kind) {
                ScopeKind.SERVER -> Scope.Server
                ScopeKind.SCHEMA -> Scope.Schema(schema.trim())
                ScopeKind.TABLE -> Scope.Table(schema.trim(), table.trim())
            }
            val ready = chosen.isNotEmpty() && (kind == ScopeKind.SERVER || schema.isNotBlank()) && (kind != ScopeKind.TABLE || table.isNotBlank())
            TextButton(enabled = ready, onClick = {
                // In the order the server lists them, not the order tapped.
                val privileges = abilities.privileges.filter { it in chosen }
                submit(if (grant) UserAction.Grant(account, privileges, scope) else UserAction.Revoke(account, privileges, scope))
            }) { Text("Review") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** A text field with the catalog's names offered beneath it. */
@Composable
private fun Picker(label: String, value: String, offered: List<String>, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
        modifier = Modifier.fillMaxWidth(),
    )
    val matching = offered.filter { it != value && it.contains(value, ignoreCase = true) }.take(12)
    if (matching.isNotEmpty()) {
        Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            matching.forEach { name -> AssistChip(onClick = { onChange(name) }, label = { Text(name, fontSize = 12.sp) }) }
        }
    }
}

@Composable
private fun RoleForm(account: Account, abilities: UserAbilities, onDismiss: () -> Unit, submit: (UserAction.GrantRole) -> Unit) {
    var role by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Grant a role to ${account.label()}") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(10.dp)) { Picker("Role", role, abilities.suggestedRoles) { role = it } } },
        confirmButton = {
            TextButton(enabled = role.isNotBlank(), onClick = { submit(UserAction.GrantRole(account, role.trim(), "")) }) { Text("Review") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/**
 * The statement exactly as it will run, before it runs. Its destructive
 * kinds are named here, so Run is the confirmation; a mode it needs still
 * goes through the gate's own dialog.
 */
@Composable
private fun ReviewDialog(model: AppViewModel, review: Review) {
    val palette = LocalPalette.current
    val verdict = remember(review) { review.prepared.verdict() }
    val destroys = verdict.destructive.isNotEmpty()
    AlertDialog(
        onDismissRequest = { model.dismissReview() },
        title = { Text(review.title) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Text(
                    review.prepared.shown(),
                    fontFamily = Mono,
                    fontSize = 12.sp,
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 240.dp)
                        .background(MaterialTheme.colorScheme.background, RoundedCornerShape(8.dp))
                        .border(1.dp, MaterialTheme.colorScheme.outlineVariant, RoundedCornerShape(8.dp))
                        .verticalScroll(rememberScrollState())
                        .padding(10.dp),
                )
                if (destroys) Text("This drops the account. It cannot be undone from here.", color = palette.red)
                if (model.mode < verdict.mode) {
                    Text(
                        "Needs ${verdict.mode.label()}; ${model.profile?.name} is ${model.mode.label()}, so you will be asked to confirm.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {
            Button(
                onClick = { model.runReviewed() },
                colors = if (destroys) ButtonDefaults.buttonColors(containerColor = palette.red) else ButtonDefaults.buttonColors(),
            ) { Text("Run") }
        },
        dismissButton = { TextButton(onClick = { model.dismissReview() }) { Text("Cancel") } },
    )
}
