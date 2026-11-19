package dev.dbdelve.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material.icons.filled.Person
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.ScrollableTabRow
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.OffsetMapping
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.input.TransformedText
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.Pane
import dev.dbdelve.android.PendingGate
import dev.dbdelve.android.QueryTab
import uniffi.dbdelve_core.CatalogEntry
import uniffi.dbdelve_core.Destructive
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.ObjectKind
import uniffi.dbdelve_core.ResultSet
import uniffi.dbdelve_core.Stop
import uniffi.dbdelve_core.holdsReadOnly
import uniffi.dbdelve_core.statementAt

/** Asks the device's own lock (fingerprint, face or PIN), then runs [onSuccess]. */
typealias Authenticate = (title: String, onSuccess: () -> Unit) -> Unit

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun WorkspaceScreen(model: AppViewModel, authenticate: Authenticate) {
    val profile = model.profile ?: return
    val palette = LocalPalette.current
    BackHandler {
        if (model.pane != Pane.QUERY) model.pane = Pane.QUERY else model.disconnect()
    }
    Scaffold(
        topBar = {
            Column {
                TopAppBar(
                    navigationIcon = {
                        IconButton(onClick = { model.disconnect() }) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Disconnect")
                        }
                    },
                    title = {
                        Column {
                            Text(profile.name, maxLines = 1, overflow = TextOverflow.Ellipsis, fontSize = 16.sp)
                            Text(
                                listOf(profile.host, profile.database).filter { it.isNotBlank() }.joinToString(" / "),
                                fontFamily = Mono,
                                fontSize = 11.sp,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    },
                    actions = { ModeChip(model, authenticate) },
                )
                // The connection's mode as a band, as the desktop colours its
                // title bar: hard to miss on the one screen that matters.
                Box(Modifier.fillMaxWidth().height(3.dp).background(palette.of(model.mode)))
            }
        },
        bottomBar = {
            if (!WindowInsets.isImeVisible) {
                NavigationBar {
                    NavigationBarItem(
                        selected = model.pane == Pane.QUERY,
                        onClick = { model.pane = Pane.QUERY },
                        icon = { Icon(Icons.Default.Edit, contentDescription = null) },
                        label = { Text("Query") },
                    )
                    NavigationBarItem(
                        selected = model.pane == Pane.RESULTS,
                        onClick = { model.pane = Pane.RESULTS },
                        icon = { Icon(Icons.AutoMirrored.Filled.List, contentDescription = null) },
                        label = { Text("Results") },
                    )
                    NavigationBarItem(
                        selected = model.pane == Pane.SCHEMA,
                        onClick = { model.pane = Pane.SCHEMA },
                        icon = { Icon(Icons.Default.Menu, contentDescription = null) },
                        label = { Text("Schema") },
                    )
                    NavigationBarItem(
                        selected = model.pane == Pane.USERS,
                        onClick = { model.pane = Pane.USERS },
                        icon = { Icon(Icons.Default.Person, contentDescription = null) },
                        label = { Text("Users") },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding).fillMaxSize()) {
            when (model.pane) {
                Pane.QUERY -> QueryPane(model)
                Pane.RESULTS -> ResultsPane(model)
                Pane.SCHEMA -> SchemaPane(model)
                Pane.USERS -> UsersPane(model)
            }
        }
    }
    model.gate?.let { GateDialog(model, it, authenticate) }
}

@Composable
private fun ModeChip(model: AppViewModel, authenticate: Authenticate) {
    val color = LocalPalette.current.of(model.mode)
    var open by remember { mutableStateOf(false) }
    Box(Modifier.padding(end = 8.dp)) {
        Row(
            Modifier
                .border(1.dp, color.copy(alpha = 0.45f), RoundedCornerShape(7.dp))
                .background(color.copy(alpha = 0.12f), RoundedCornerShape(7.dp))
                .clickable { open = true }
                .padding(horizontal = 10.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            if (model.mode == Mode.READ_ONLY) Icon(Icons.Default.Lock, null, tint = color, modifier = Modifier.size(13.dp))
            Text(model.mode.label(), color = color, fontSize = 12.sp, fontWeight = FontWeight.SemiBold)
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            Mode.entries.forEach { mode ->
                DropdownMenuItem(
                    text = { Text(mode.label()) },
                    onClick = {
                        open = false
                        // Raising a connection to Full asks the device first:
                        // on a phone the risk is a stray tap, not a typo.
                        if (mode == Mode.FULL && model.mode != Mode.FULL) {
                            authenticate("Switch ${model.profile?.name} to Full") { model.setMode(mode) }
                        } else {
                            model.setMode(mode)
                        }
                    },
                )
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun QueryPane(model: AppViewModel) {
    val session = model.session ?: return
    val engine = session.engine()
    val palette = LocalPalette.current
    val tab = model.tab ?: return
    Column(Modifier.fillMaxSize().imePadding()) {
        ScrollableTabRow(selectedTabIndex = model.current, edgePadding = 8.dp) {
            model.tabs.forEachIndexed { at, each ->
                Tab(
                    selected = at == model.current,
                    onClick = { model.current = at },
                    text = {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(each.title, maxLines = 1)
                            if (at == model.current) {
                                Icon(
                                    Icons.Default.Close,
                                    contentDescription = "Close ${each.title}",
                                    modifier = Modifier.padding(start = 6.dp).size(14.dp).clickable { model.closeTab(each) },
                                )
                            }
                        }
                    },
                )
            }
            Tab(selected = false, onClick = { model.newTab() }, icon = { Icon(Icons.Default.Add, contentDescription = "New query") })
        }

        BasicTextField(
            value = tab.text,
            onValueChange = { tab.text = it },
            textStyle = TextStyle(fontFamily = Mono, fontSize = 14.sp, lineHeight = 21.sp, color = MaterialTheme.colorScheme.onSurface),
            cursorBrush = SolidColor(palette.orange),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii, autoCorrectEnabled = false),
            visualTransformation = remember(palette) { Highlight(palette) },
            modifier = Modifier.weight(1f).fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
            decorationBox = { field ->
                Box {
                    if (tab.text.text.isEmpty()) {
                        Text(
                            if (engine == uniffi.dbdelve_core.Engine.MONGO) "db.collection.find({})" else "SELECT …",
                            fontFamily = Mono,
                            fontSize = 14.sp,
                            color = palette.faint,
                        )
                    }
                    field()
                }
            },
        )

        // What Run will send: the statement under the cursor, as typed.
        val span = statementAt(engine, tab.text.text, tab.text.selection.start.toUInt())
        val next = span?.let { tab.text.text.substring(it.start.toInt(), it.end.toInt()) }
        Text(
            next?.let { "Runs: " + it.lineSequence().first().take(60) + if (it.contains('\n') || it.length > 60) " …" else "" }
                ?: "Nothing to run",
            fontFamily = Mono,
            fontSize = 11.sp,
            color = palette.faint,
            maxLines = 1,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 2.dp),
        )

        Row(
            Modifier.fillMaxWidth().padding(horizontal = 10.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            RowLimit(model)
            Spacer(Modifier.weight(1f))
            if (model.running) {
                CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                FilledTonalButton(onClick = { model.cancel() }) { Text("Cancel") }
            } else {
                Button(onClick = { model.runAtCursor() }, enabled = next != null) {
                    Icon(Icons.Default.PlayArrow, contentDescription = null, modifier = Modifier.size(16.dp))
                    Text("Run", modifier = Modifier.padding(start = 4.dp))
                }
            }
        }
        AccessoryKeys(tab)
    }
}

@Composable
private fun RowLimit(model: AppViewModel) {
    var open by remember { mutableStateOf(false) }
    Box {
        TextButton(onClick = { open = true }) { Text("%,d rows ▾".format(model.rowLimit)) }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            listOf(100, 1_000, 10_000).forEach { limit ->
                DropdownMenuItem(text = { Text("%,d rows".format(limit)) }, onClick = { model.rowLimit = limit; open = false })
            }
        }
    }
}

/** The keys a soft keyboard buries two layers deep. */
@Composable
private fun AccessoryKeys(tab: QueryTab) {
    val keys = listOf("⇥", "(", ")", "'", "\"", ";", "*", "=", ".", ",", "{", "}", "[", "]", ":", "$", "_", "←", "→")
    Row(
        Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceContainerHigh)
            .horizontalScroll(rememberScrollState())
            .padding(6.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        keys.forEach { key ->
            Surface(
                color = MaterialTheme.colorScheme.surfaceContainerHighest,
                shape = RoundedCornerShape(6.dp),
                modifier = Modifier.clickable { tab.text = press(tab.text, key) },
            ) {
                Text(
                    key,
                    fontFamily = Mono,
                    fontSize = 14.sp,
                    color = if (key in listOf("⇥", "←", "→")) LocalPalette.current.orange else MaterialTheme.colorScheme.onSurface,
                    modifier = Modifier.padding(horizontal = 11.dp, vertical = 8.dp),
                )
            }
        }
    }
}

private fun press(value: TextFieldValue, key: String): TextFieldValue {
    val selection = value.selection
    return when (key) {
        "←" -> value.copy(selection = TextRange((selection.min - 1).coerceAtLeast(0)))
        "→" -> value.copy(selection = TextRange((selection.max + 1).coerceAtMost(value.text.length)))
        else -> {
            val insert = if (key == "⇥") "  " else key
            val text = value.text.replaceRange(selection.min, selection.max, insert)
            TextFieldValue(text, TextRange(selection.min + insert.length))
        }
    }
}

private val KEYWORDS = setOf(
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "IS", "NULL", "AS", "ON", "JOIN", "LEFT", "RIGHT",
    "INNER", "OUTER", "FULL", "CROSS", "GROUP", "BY", "ORDER", "HAVING", "LIMIT", "OFFSET", "UNION", "ALL",
    "DISTINCT", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "ALTER", "DROP", "TABLE",
    "VIEW", "INDEX", "TRUNCATE", "WITH", "CASE", "WHEN", "THEN", "ELSE", "END", "ASC", "DESC", "LIKE",
    "BETWEEN", "EXISTS", "EXPLAIN", "ANALYZE", "BEGIN", "COMMIT", "ROLLBACK", "RETURNING", "TRUE", "FALSE",
    "PRIMARY", "KEY", "DEFAULT", "SHOW", "USE", "GRANT", "REVOKE",
)

private val TOKEN = Regex(
    """(--[^\n]*|/\*[\s\S]*?(\*/|$))|('(?:[^'\\]|\\.|'')*'?|"(?:[^"\\]|\\.)*"?)|(\$?[A-Za-z_][A-Za-z0-9_]*)|(\b\d+(?:\.\d+)?\b)""",
)

/** Colours the buffer without changing a character of it. */
private class Highlight(private val palette: Palette) : VisualTransformation {
    override fun filter(text: AnnotatedString): TransformedText {
        val styled = buildAnnotatedString {
            append(text.text)
            TOKEN.findAll(text.text).forEach { match ->
                val range = match.range
                val style = when {
                    match.groups[1] != null -> SpanStyle(color = palette.faint, fontStyle = FontStyle.Italic)
                    match.groups[3] != null -> SpanStyle(color = palette.green)
                    match.groups[4] != null -> {
                        val word = match.value
                        when {
                            word.startsWith("$") -> SpanStyle(color = palette.aqua)
                            word.uppercase() in KEYWORDS -> SpanStyle(color = palette.red)
                            else -> null
                        }
                    }
                    match.groups[5] != null -> SpanStyle(color = palette.purple)
                    else -> null
                }
                style?.let { addStyle(it, range.first, range.last + 1) }
            }
        }
        return TransformedText(styled, OffsetMapping.Identity)
    }
}

@Composable
private fun ResultsPane(model: AppViewModel) {
    val tab = model.tab
    val palette = LocalPalette.current
    val error = tab?.error
    val result = tab?.result
    when {
        tab == null || (error == null && result == null) -> Centered("Run a statement to see its rows here.")
        error != null -> Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp)) {
            Text("The server returned an error", color = palette.red, fontWeight = FontWeight.SemiBold)
            Text(error, fontFamily = Mono, fontSize = 13.sp, modifier = Modifier.padding(vertical = 10.dp))
            tab?.ran?.let { Statement(it) }
        }
        result != null -> Rows(result)
    }
}

@Composable
private fun Rows(result: ResultSet) {
    val palette = LocalPalette.current
    val clipboard = LocalClipboardManager.current
    var records by remember { mutableStateOf(false) }
    var opened by remember { mutableStateOf<Pair<String, String?>?>(null) }
    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth().padding(10.dp), verticalAlignment = Alignment.CenterVertically) {
            SingleChoiceSegmentedButtonRow {
                SegmentedButton(selected = !records, onClick = { records = false }, shape = SegmentedButtonDefaults.itemShape(0, 2)) { Text("Grid") }
                SegmentedButton(selected = records, onClick = { records = true }, shape = SegmentedButtonDefaults.itemShape(1, 2)) { Text("Records") }
            }
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { clipboard.setText(AnnotatedString(csv(result))) }) { Text("Copy CSV") }
        }
        Box(Modifier.weight(1f)) {
            when {
                result.columns.isEmpty() -> Centered(result.affected?.let { "$it rows affected" } ?: "Done")
                records -> Records(result) { column, value -> opened = column to value }
                else -> Grid(result) { column, value -> opened = column to value }
            }
        }
        Row(
            Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surfaceContainer).padding(horizontal = 12.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            // The limit bounded what was kept, never what was sent; say when it did.
            Text(
                "%,d rows".format(result.rows.size) + if (result.capped) " · stopped at limit" else "",
                fontSize = 12.sp,
                fontWeight = if (result.capped) FontWeight.SemiBold else FontWeight.Normal,
                color = if (result.capped) palette.yellow else MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text("${result.elapsedMs} ms", fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
    opened?.let { (column, value) ->
        AlertDialog(
            onDismissRequest = { opened = null },
            title = { Text(column, fontFamily = Mono) },
            text = {
                Text(
                    value ?: "NULL",
                    fontFamily = Mono,
                    fontStyle = if (value == null) FontStyle.Italic else FontStyle.Normal,
                    modifier = Modifier.heightIn(max = 360.dp).verticalScroll(rememberScrollState()),
                )
            },
            confirmButton = {
                TextButton(onClick = { value?.let { clipboard.setText(AnnotatedString(it)) }; opened = null }) { Text("Copy") }
            },
            dismissButton = { TextButton(onClick = { opened = null }) { Text("Close") } },
        )
    }
}

private const val CHAR_DP = 8.4f

@Composable
private fun Grid(result: ResultSet, open: (String, String?) -> Unit) {
    val palette = LocalPalette.current
    // Wide enough for what most of the column holds, not for its longest value.
    val widths = remember(result) {
        result.columns.mapIndexed { at, column ->
            val longest = result.rows.asSequence().take(200).maxOfOrNull { (it.getOrNull(at) ?: "NULL").length } ?: 0
            (maxOf(column.name.length + column.typeTag.length + 1, longest).coerceIn(4, 36) * CHAR_DP + 20).dp
        }
    }
    val numberWidth = ((result.rows.size.toString().length).coerceAtLeast(2) * CHAR_DP + 16).dp
    // One horizontal position for the header and every row; the row numbers
    // stay put beside them.
    val across: ScrollState = rememberScrollState()
    LazyColumn(Modifier.fillMaxSize()) {
        stickyHeaderRow(result, widths, numberWidth, across)
        itemsIndexed(result.rows) { index, row ->
            Row(Modifier.height(34.dp)) {
                Cell("${index + 1}", numberWidth, palette.faint)
                Row(Modifier.horizontalScroll(across)) {
                    row.forEachIndexed { at, value ->
                        val numeric = value?.toDoubleOrNull() != null
                        Box(
                            Modifier
                                .width(widths[at])
                                .height(34.dp)
                                .clickable { open(result.columns[at].name, value) }
                                .padding(horizontal = 10.dp),
                            contentAlignment = Alignment.CenterStart,
                        ) {
                            Text(
                                value ?: "NULL",
                                fontFamily = Mono,
                                fontSize = 12.sp,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                fontStyle = if (value == null) FontStyle.Italic else FontStyle.Normal,
                                color = when {
                                    value == null -> palette.faint
                                    numeric -> palette.yellow
                                    else -> MaterialTheme.colorScheme.onSurface
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
private fun androidx.compose.foundation.lazy.LazyListScope.stickyHeaderRow(
    result: ResultSet,
    widths: List<androidx.compose.ui.unit.Dp>,
    numberWidth: androidx.compose.ui.unit.Dp,
    across: ScrollState,
) {
    stickyHeader {
        val palette = LocalPalette.current
        Row(Modifier.height(36.dp).background(MaterialTheme.colorScheme.surfaceContainer)) {
            Cell("#", numberWidth, palette.faint)
            Row(Modifier.horizontalScroll(across)) {
                result.columns.forEachIndexed { at, column ->
                    Row(
                        Modifier.width(widths[at]).height(36.dp).padding(horizontal = 10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(column.name, fontFamily = Mono, fontSize = 12.sp, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
                        if (column.typeTag.isNotEmpty()) {
                            Text(" ${column.typeTag}", fontFamily = Mono, fontSize = 9.sp, color = palette.faint, maxLines = 1)
                        }
                    }
                }
            }
        }
    }
}

/** The pinned row-number column. */
@Composable
private fun Cell(text: String, width: androidx.compose.ui.unit.Dp, color: androidx.compose.ui.graphics.Color) {
    Box(
        Modifier
            .width(width)
            .height(34.dp)
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .padding(horizontal = 6.dp),
        contentAlignment = Alignment.CenterEnd,
    ) {
        Text(text, fontFamily = Mono, fontSize = 12.sp, color = color, maxLines = 1)
    }
}

@Composable
private fun Records(result: ResultSet, open: (String, String?) -> Unit) {
    val palette = LocalPalette.current
    LazyColumn(Modifier.fillMaxSize().padding(horizontal = 10.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        itemsIndexed(result.rows) { index, row ->
            Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(8.dp)) {
                Column(Modifier.fillMaxWidth().padding(10.dp)) {
                    Text("#${index + 1}", fontFamily = Mono, fontSize = 11.sp, color = palette.faint)
                    row.forEachIndexed { at, value ->
                        Row(Modifier.fillMaxWidth().clickable { open(result.columns[at].name, value) }.padding(vertical = 2.dp)) {
                            Text(result.columns[at].name, fontFamily = Mono, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.width(120.dp), maxLines = 1, overflow = TextOverflow.Ellipsis)
                            Text(
                                value ?: "NULL",
                                fontFamily = Mono,
                                fontSize = 12.sp,
                                fontStyle = if (value == null) FontStyle.Italic else FontStyle.Normal,
                                color = if (value == null) palette.faint else MaterialTheme.colorScheme.onSurface,
                                maxLines = 3,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                }
            }
        }
    }
}

private fun csv(result: ResultSet): String {
    fun field(value: String?): String = when {
        value == null -> ""
        value.any { it == ',' || it == '"' || it == '\n' || it == '\r' } -> "\"" + value.replace("\"", "\"\"") + "\""
        else -> value
    }
    return buildString {
        appendLine(result.columns.joinToString(",") { field(it.name) })
        result.rows.forEach { row -> appendLine(row.joinToString(",") { field(it) }) }
    }
}

@Composable
private fun SchemaPane(model: AppViewModel) {
    var filter by remember { mutableStateOf("") }
    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = filter,
                onValueChange = { filter = it },
                leadingIcon = { Icon(Icons.Default.Search, null) },
                placeholder = { Text("Filter objects") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = { model.refreshCatalog() }) { Icon(Icons.Default.Refresh, contentDescription = "Refresh") }
        }
        model.catalogError?.let { Text(it, color = LocalPalette.current.red, fontFamily = Mono, fontSize = 12.sp, modifier = Modifier.padding(12.dp)) }
        val shown = model.catalog.filter { filter.isBlank() || it.name.contains(filter.trim(), ignoreCase = true) }
        LazyColumn(Modifier.fillMaxSize()) {
            shown.groupBy { it.schema }.forEach { (schema, entries) ->
                item(key = "schema:$schema") { SectionHeader(schema, entries.size) }
                items(entries, key = { "${it.schema}.${it.name}" }) { entry -> CatalogRow(model, entry) }
            }
        }
    }
}

@Composable
private fun CatalogRow(model: AppViewModel, entry: CatalogEntry) {
    val palette = LocalPalette.current
    val clipboard = LocalClipboardManager.current
    var open by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier.fillMaxWidth().clickable { open = true }.padding(horizontal = 16.dp, vertical = 11.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                when (entry.kind) {
                    ObjectKind.TABLE -> "T"
                    ObjectKind.VIEW -> "V"
                    ObjectKind.COLLECTION -> "C"
                },
                fontFamily = Mono,
                fontSize = 11.sp,
                fontWeight = FontWeight.Bold,
                color = palette.blue,
            )
            Text(entry.name, modifier = Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
            entry.sizeBytes?.let { Text(size(it.toLong()), fontFamily = Mono, fontSize = 11.sp, color = palette.faint) }
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(text = { Text("Preview rows") }, onClick = { open = false; model.preview(entry) })
            DropdownMenuItem(text = { Text("Copy name") }, onClick = { open = false; clipboard.setText(AnnotatedString(entry.name)) })
        }
    }
}

private fun size(bytes: Long): String = when {
    bytes >= 1L shl 30 -> "%.1f GB".format(bytes / (1L shl 30).toDouble())
    bytes >= 1L shl 20 -> "%.1f MB".format(bytes / (1L shl 20).toDouble())
    else -> "%.1f KB".format(bytes / 1024.0)
}

@Composable
private fun GateDialog(model: AppViewModel, pending: PendingGate, authenticate: Authenticate) {
    val palette = LocalPalette.current
    val name = model.profile?.name ?: ""
    val engine = model.session?.engine()
    val stop = pending.stop
    AlertDialog(
        onDismissRequest = { model.dismissGate() },
        title = {
            Text(
                when (stop) {
                    is Stop.Upgrade -> "This statement needs ${stop.mode.label()}"
                    is Stop.Confirm -> "This statement ${destroys(stop.kind)}"
                    Stop.RunOnce -> "DBDelve can't read this statement"
                },
            )
        },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Text(
                    when (stop) {
                        is Stop.Upgrade -> "$name is ${model.mode.label()}." +
                            if (pending.verdict.destructive.isNotEmpty()) " It ${pending.verdict.destructive.joinToString(" and ") { destroys(it) }}." else ""
                        is Stop.Confirm -> "Check the statement below before it runs on $name."
                        Stop.RunOnce ->
                            "It may be a typo, or something this app can't classify. " +
                                if (model.mode == Mode.READ_ONLY && engine != null && holdsReadOnly(engine)) {
                                    "The server still holds $name to reads."
                                } else {
                                    "Nothing but you stops it writing."
                                }
                    },
                )
                Statement(pending.statement)
            }
        },
        confirmButton = {
            Column(horizontalAlignment = Alignment.End) {
                when (stop) {
                    is Stop.Upgrade -> {
                        TextButton(onClick = { authenticate("Run once in ${stop.mode.label()} on $name") { model.runOnceIn(stop.mode) } }) {
                            Text("Run once in ${stop.mode.label()}")
                        }
                        TextButton(onClick = {
                            if (stop.mode == Mode.FULL) {
                                authenticate("Switch $name to Full") { model.switchAndRun(stop.mode) }
                            } else {
                                model.switchAndRun(stop.mode)
                            }
                        }) { Text("Switch $name to ${stop.mode.label()}") }
                    }
                    is Stop.Confirm -> Button(
                        onClick = { model.confirm(stop.kind) },
                        colors = ButtonDefaults.buttonColors(containerColor = palette.red),
                    ) { Text("Run it") }
                    Stop.RunOnce -> TextButton(onClick = { model.runUnreadableOnce() }) { Text("Run once") }
                }
            }
        },
        dismissButton = { TextButton(onClick = { model.dismissGate() }) { Text("Cancel") } },
    )
}

private fun destroys(kind: Destructive): String = when (kind) {
    Destructive.DROP -> "drops an object and its data"
    Destructive.TRUNCATE -> "empties a table"
    Destructive.UNFILTERED_DELETE -> "deletes every row"
    Destructive.UNREADABLE -> "could not be read"
}

/** A statement verbatim: what is approved is the text, not a summary of it. */
@Composable
private fun Statement(text: String) {
    Text(
        text,
        fontFamily = Mono,
        fontSize = 12.sp,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(max = 220.dp)
            .background(MaterialTheme.colorScheme.background, RoundedCornerShape(8.dp))
            .border(1.dp, MaterialTheme.colorScheme.outlineVariant, RoundedCornerShape(8.dp))
            .verticalScroll(rememberScrollState())
            .padding(10.dp),
    )
}

@Composable
private fun Centered(text: String) {
    Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
        Text(text, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}
