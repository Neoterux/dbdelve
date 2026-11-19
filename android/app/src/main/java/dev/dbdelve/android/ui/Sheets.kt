package dev.dbdelve.android.ui

import android.content.Context
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.data.SavedQuery
import uniffi.dbdelve_core.Binding
import uniffi.dbdelve_core.ParamValue
import uniffi.dbdelve_core.ResultSet
import uniffi.dbdelve_core.parameters

/** One tappable line of a bottom sheet: a sheet is a list of these. */
@Composable
fun SheetAction(
    text: String,
    color: Color = MaterialTheme.colorScheme.onSurface,
    detail: String? = null,
    icon: ImageVector? = null,
    onClick: () -> Unit,
) {
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 24.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        icon?.let { Icon(it, contentDescription = null, tint = if (color == MaterialTheme.colorScheme.onSurface) MaterialTheme.colorScheme.onSurfaceVariant else color) }
        Column(Modifier.weight(1f)) {
            Text(text, color = color, fontSize = 16.sp)
            detail?.let { Text(it, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2, overflow = TextOverflow.Ellipsis) }
        }
    }
}

@Composable
fun SheetTitle(text: String, detail: String? = null) {
    Column(Modifier.fillMaxWidth().padding(start = 24.dp, end = 24.dp, bottom = 8.dp)) {
        Text(text, style = MaterialTheme.typography.titleMedium, maxLines = 2, overflow = TextOverflow.Ellipsis)
        detail?.let { Text(it, fontFamily = Mono, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2, overflow = TextOverflow.Ellipsis) }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SavedQueriesSheet(model: AppViewModel, onDismiss: () -> Unit) {
    var filter by remember { mutableStateOf("") }
    var running by remember { mutableStateOf<SavedQuery?>(null) }
    var confirmDelete by remember { mutableStateOf<SavedQuery?>(null) }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        SheetTitle("Saved queries", "${model.profile?.name}: :name marks a parameter")
        OutlinedTextField(
            value = filter,
            onValueChange = { filter = it },
            placeholder = { Text("Search") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp),
        )
        val shown = model.saved.filter { filter.isBlank() || it.name.contains(filter.trim(), ignoreCase = true) }
        if (model.saved.isEmpty()) {
            Text(
                "Nothing saved for this connection yet. Save a query from the editor or the builder with the bookmark button.",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(24.dp),
            )
        }
        LazyColumn(Modifier.heightIn(max = 480.dp).navigationBarsPadding()) {
            items(shown, key = { it.id }) { query ->
                val engine = model.session?.engine()
                val names = remember(query.text, engine) { engine?.let { parameters(it, query.text) }.orEmpty() }
                Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp)) {
                    Text(query.name, fontWeight = FontWeight.SemiBold)
                    Text(query.text.lineSequence().first().take(80), fontFamily = Mono, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    if (names.isNotEmpty()) Text(names.joinToString("  ") { ":$it" }, fontFamily = Mono, fontSize = 11.sp, color = LocalPalette.current.aqua)
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        FilledTonalButton(onClick = {
                            if (names.isEmpty()) {
                                model.runSaved(query, emptyList())
                                onDismiss()
                            } else {
                                running = query
                            }
                        }) {
                            Icon(Icons.Default.PlayArrow, null, Modifier.size(18.dp))
                            Text("Run", Modifier.padding(start = 6.dp))
                        }
                        IconButton(onClick = { model.openSaved(query); onDismiss() }) {
                            Icon(Icons.Default.Edit, contentDescription = "Open ${query.name} in a tab")
                        }
                        Spacer(Modifier.weight(1f))
                        IconButton(onClick = { confirmDelete = query }) {
                            Icon(Icons.Default.Delete, contentDescription = "Delete ${query.name}", tint = LocalPalette.current.red)
                        }
                    }
                }
                HorizontalDivider()
            }
        }
    }
    running?.let { query ->
        ParamsDialog(model, query, onDismiss = { running = null }) {
            running = null
            onDismiss()
        }
    }
    confirmDelete?.let { query ->
        AlertDialog(
            onDismissRequest = { confirmDelete = null },
            title = { Text("Delete ${query.name}?") },
            text = { Text("The saved query goes; nothing on the server changes.") },
            confirmButton = { TextButton(onClick = { model.deleteSaved(query); confirmDelete = null }) { Text("Delete") } },
            dismissButton = { TextButton(onClick = { confirmDelete = null }) { Text("Cancel") } },
        )
    }
}

private enum class Kind { TEXT, NUMBER, NULL }

/**
 * A value per parameter, and what the query becomes with them, before it
 * runs: the filled statement is what the gate reads and what runs.
 */
@Composable
private fun ParamsDialog(model: AppViewModel, query: SavedQuery, onDismiss: () -> Unit, ran: () -> Unit) {
    val engine = model.session?.engine() ?: return
    val names = remember(query.text) { parameters(engine, query.text) }
    val values = remember { mutableStateMapOf<String, String>() }
    val kinds = remember { mutableStateMapOf<String, Kind>() }
    var error by remember { mutableStateOf<String?>(null) }
    val bindings = names.map { name ->
        Binding(
            name,
            when (kinds[name] ?: Kind.TEXT) {
                Kind.TEXT -> ParamValue.Text(values[name].orEmpty())
                Kind.NUMBER -> ParamValue.Number(values[name].orEmpty())
                Kind.NULL -> ParamValue.Null
            },
        )
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(query.name) },
        text = {
            Column(Modifier.heightIn(max = 520.dp).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                names.forEach { name ->
                    val kind = kinds[name] ?: Kind.TEXT
                    Text(":$name", fontFamily = Mono, fontWeight = FontWeight.SemiBold)
                    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                        Kind.entries.forEachIndexed { at, each ->
                            SegmentedButton(
                                selected = kind == each,
                                onClick = { kinds[name] = each },
                                shape = SegmentedButtonDefaults.itemShape(at, Kind.entries.size),
                            ) { Text(each.name.lowercase().replaceFirstChar { it.uppercase() }, fontSize = 12.sp) }
                        }
                    }
                    if (kind != Kind.NULL) {
                        OutlinedTextField(
                            value = values[name].orEmpty(),
                            onValueChange = { values[name] = it },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(
                                keyboardType = if (kind == Kind.NUMBER) KeyboardType.Decimal else KeyboardType.Text,
                                autoCorrectEnabled = false,
                            ),
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                }
                val preview = runCatching { uniffi.dbdelve_core.fillParameters(engine, query.text, bindings) }
                Text("Runs as", style = MaterialTheme.typography.labelLarge)
                Text(
                    preview.getOrElse { it.reason() },
                    fontFamily = Mono,
                    fontSize = 12.sp,
                    color = if (preview.isFailure) LocalPalette.current.red else MaterialTheme.colorScheme.onSurface,
                )
                error?.let { Text(it, color = LocalPalette.current.red) }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                error = model.runSaved(query, bindings)
                if (error == null) ran()
            }) { Text("Run") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

private fun Throwable.reason(): String = when (this) {
    is uniffi.dbdelve_core.CoreException.Query -> reason
    is uniffi.dbdelve_core.CoreException.Connect -> reason
    else -> message ?: toString()
}

@Composable
fun SaveQueryDialog(initial: String, onDismiss: () -> Unit, save: (String) -> Unit) {
    var name by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Save query") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(value = name, onValueChange = { name = it }, label = { Text("Name") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                Text(
                    "Write :name where a value goes, e.g. WHERE id = :id. Running it asks for each value and shows the statement it makes.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton(enabled = name.isNotBlank(), onClick = { save(name.trim()) }) { Text("Save") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Every field of one row, each copyable: tapping a cell or record opens it. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RowSheet(result: ResultSet, index: Int, copy: (String) -> Unit, onDismiss: () -> Unit) {
    val row = result.rows.getOrNull(index) ?: return
    val palette = LocalPalette.current
    ModalBottomSheet(onDismissRequest = onDismiss) {
        SheetTitle("Row ${index + 1} of ${result.rows.size}")
        LazyColumn(Modifier.heightIn(max = 560.dp).navigationBarsPadding()) {
            items(result.columns.size) { at ->
                val value = row.getOrNull(at)
                Column(
                    Modifier
                        .fillMaxWidth()
                        .clickable { value?.let(copy) }
                        .padding(horizontal = 24.dp, vertical = 10.dp),
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(result.columns[at].name, fontFamily = Mono, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.weight(1f))
                        Text(result.columns[at].typeTag, fontFamily = Mono, fontSize = 10.sp, color = palette.faint)
                    }
                    Text(
                        value ?: "NULL",
                        fontFamily = Mono,
                        fontSize = 14.sp,
                        fontStyle = if (value == null) FontStyle.Italic else FontStyle.Normal,
                        color = if (value == null) palette.faint else MaterialTheme.colorScheme.onSurface,
                    )
                }
            }
            item {
                Text(
                    "Tap a value to copy it.",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(24.dp),
                )
            }
        }
    }
}

/** The changelog packaged into the app (CHANGELOG.md at the repository root). */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChangelogSheet(onDismiss: () -> Unit) {
    val context = LocalContext.current
    val lines = remember {
        runCatching { context.assets.open("CHANGELOG.md").bufferedReader().readLines() }.getOrDefault(listOf("No changelog in this build."))
    }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        SheetTitle("What's new")
        LazyColumn(Modifier.heightIn(max = 600.dp).navigationBarsPadding().padding(horizontal = 24.dp)) {
            items(lines.dropWhile { !it.startsWith("## ") }) { line ->
                when {
                    line.startsWith("## ") -> Text(line.removePrefix("## "), style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 16.dp))
                    line.startsWith("### ") -> Text(line.removePrefix("### "), style = MaterialTheme.typography.labelLarge, color = LocalPalette.current.orange, modifier = Modifier.padding(top = 8.dp))
                    line.startsWith("- ") -> Text("• " + line.removePrefix("- ").replace("`", ""), fontSize = 14.sp, modifier = Modifier.padding(top = 2.dp))
                    line.isBlank() -> Spacer(Modifier.size(4.dp))
                    else -> Text(line.replace("`", ""), fontSize = 14.sp)
                }
            }
        }
    }
}

/**
 * Hands rows to any app that takes a CSV (mail, drive, a spreadsheet)
 * through Android's share sheet: a file, so no size limit like the
 * clipboard's.
 */
fun shareCsv(context: Context, name: String, csv: String) {
    val safe = name.replace(Regex("[^A-Za-z0-9._-]"), "_").ifBlank { "results" }.take(60)
    shareFile(context, "$safe.csv", csv.toByteArray(), "text/csv")
}
