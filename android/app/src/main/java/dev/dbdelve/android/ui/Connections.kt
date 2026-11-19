package dev.dbdelve.android.ui

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Snackbar
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
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.dbdelve.android.AppViewModel
import dev.dbdelve.android.Screen
import dev.dbdelve.android.data.Profile
import uniffi.dbdelve_core.Mode

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectionsScreen(model: AppViewModel) {
    var filter by remember { mutableStateOf("") }
    Scaffold(
        topBar = { TopAppBar(title = { Text("DBDelve", fontWeight = FontWeight.Bold) }) },
        floatingActionButton = {
            ExtendedFloatingActionButton(
                onClick = { model.screen = Screen.Edit(null) },
                icon = { Icon(Icons.Default.Add, contentDescription = null) },
                text = { Text("Connection") },
            )
        },
        snackbarHost = {
            model.connectError?.let { error ->
                Snackbar(
                    modifier = Modifier.padding(12.dp),
                    action = { TextButton(onClick = { model.connectError = null }) { Text("Dismiss") } },
                ) { Text(error) }
            }
        },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize()) {
            OutlinedTextField(
                value = filter,
                onValueChange = { filter = it },
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
                leadingIcon = { Icon(Icons.Default.Search, contentDescription = null) },
                placeholder = { Text("Search connections") },
                singleLine = true,
            )
            val shown = model.profiles.filter {
                filter.isBlank() || listOf(it.name, it.host, it.database, it.project)
                    .any { field -> field.contains(filter.trim(), ignoreCase = true) }
            }
            if (model.profiles.isEmpty()) {
                Text(
                    "No connections yet. Add one to start; it opens Read-only.",
                    modifier = Modifier.padding(24.dp),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            LazyColumn(Modifier.fillMaxSize()) {
                shown.groupBy { it.project.ifBlank { "Connections" } }.forEach { (project, profiles) ->
                    item(key = "project:$project") { SectionHeader(project, profiles.size) }
                    items(profiles, key = { it.id }) { profile ->
                        ProfileRow(profile, connecting = model.connecting == profile.id, model = model)
                    }
                }
                item { Spacer(Modifier.size(96.dp)) }
            }
        }
    }
}

@Composable
fun SectionHeader(title: String, count: Int? = null) {
    Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 14.dp, bottom = 6.dp)) {
        Text(
            title.uppercase(),
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.weight(1f),
        )
        count?.let { Text("$it", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant) }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ProfileRow(profile: Profile, connecting: Boolean, model: AppViewModel) {
    val palette = LocalPalette.current
    var menu by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier
                .fillMaxWidth()
                .combinedClickable(onClick = { model.connect(profile) }, onLongClick = { menu = true })
                .padding(horizontal = 14.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Box(
                Modifier
                    .size(34.dp)
                    .background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(8.dp))
                    .border(1.dp, palette.rule, RoundedCornerShape(8.dp)),
                contentAlignment = Alignment.Center,
            ) {
                Text(profile.engine.badge(), fontFamily = Mono, fontSize = 11.sp, fontWeight = FontWeight.Bold, color = palette.of(profile.engine))
            }
            Column(Modifier.weight(1f)) {
                Text(profile.name, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    "${profile.host}:${profile.port}" + if (profile.database.isNotBlank()) " / ${profile.database}" else "",
                    fontFamily = Mono,
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (connecting) {
                CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
            }
            ModeBadge(profile.mode)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text("Edit") }, onClick = { menu = false; model.screen = Screen.Edit(profile) })
            DropdownMenuItem(text = { Text("Duplicate") }, onClick = { menu = false; model.screen = Screen.Edit(profile, duplicate = true) })
            DropdownMenuItem(text = { Text("Remove ${profile.name}") }, onClick = { menu = false; model.deleteProfile(profile) })
        }
    }
}

@Composable
fun ModeBadge(mode: Mode) {
    val color = LocalPalette.current.of(mode)
    Text(
        mode.short(),
        fontFamily = Mono,
        fontSize = 10.sp,
        fontWeight = FontWeight.Bold,
        color = color,
        modifier = Modifier
            .background(color.copy(alpha = 0.14f), RoundedCornerShape(5.dp))
            .padding(horizontal = 6.dp, vertical = 3.dp),
    )
}
