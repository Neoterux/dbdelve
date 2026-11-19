package dev.dbdelve.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import uniffi.dbdelve_core.Engine
import uniffi.dbdelve_core.Mode

/** Gruvbox, the desktop's default, in the roles the views ask for. */
@Immutable
data class Palette(
    val red: Color,
    val green: Color,
    val yellow: Color,
    val blue: Color,
    val purple: Color,
    val aqua: Color,
    val orange: Color,
    val dim: Color,
    val faint: Color,
    val rule: Color,
)

private val dark = Palette(
    red = Color(0xFFFB4934),
    green = Color(0xFFB8BB26),
    yellow = Color(0xFFFABD2F),
    blue = Color(0xFF83A598),
    purple = Color(0xFFD3869B),
    aqua = Color(0xFF8EC07C),
    orange = Color(0xFFFE8019),
    dim = Color(0xFFA89984),
    faint = Color(0xFF7C6F64),
    rule = Color(0xFF504945),
)

private val light = Palette(
    red = Color(0xFF9D0006),
    green = Color(0xFF79740E),
    yellow = Color(0xFFB57614),
    blue = Color(0xFF076678),
    purple = Color(0xFF8F3F71),
    aqua = Color(0xFF427B58),
    orange = Color(0xFFAF3A03),
    dim = Color(0xFF665C54),
    faint = Color(0xFF928374),
    rule = Color(0xFFBDAE93),
)

val LocalPalette = staticCompositionLocalOf { dark }

val Mono = FontFamily.Monospace

@Composable
fun DBDelveTheme(content: @Composable () -> Unit) {
    val isDark = isSystemInDarkTheme()
    val scheme = if (isDark) {
        darkColorScheme(
            primary = dark.orange,
            onPrimary = Color(0xFF1D2021),
            background = Color(0xFF1D2021),
            onBackground = Color(0xFFEBDBB2),
            surface = Color(0xFF1D2021),
            onSurface = Color(0xFFEBDBB2),
            surfaceVariant = Color(0xFF32302F),
            onSurfaceVariant = Color(0xFFA89984),
            surfaceContainer = Color(0xFF282828),
            surfaceContainerHigh = Color(0xFF32302F),
            surfaceContainerHighest = Color(0xFF3C3836),
            secondaryContainer = Color(0xFF504945),
            onSecondaryContainer = Color(0xFFEBDBB2),
            outline = dark.rule,
            outlineVariant = Color(0xFF3C3836),
            error = dark.red,
        )
    } else {
        lightColorScheme(
            primary = light.orange,
            onPrimary = Color(0xFFFBF1C7),
            background = Color(0xFFFBF1C7),
            onBackground = Color(0xFF282828),
            surface = Color(0xFFFBF1C7),
            onSurface = Color(0xFF282828),
            surfaceVariant = Color(0xFFEBDBB2),
            onSurfaceVariant = Color(0xFF665C54),
            surfaceContainer = Color(0xFFF2E5BC),
            surfaceContainerHigh = Color(0xFFEBDBB2),
            surfaceContainerHighest = Color(0xFFD5C4A1),
            secondaryContainer = Color(0xFFD5C4A1),
            onSecondaryContainer = Color(0xFF282828),
            outline = light.rule,
            outlineVariant = Color(0xFFD5C4A1),
            error = light.red,
        )
    }
    androidx.compose.runtime.CompositionLocalProvider(LocalPalette provides if (isDark) dark else light) {
        MaterialTheme(colorScheme = scheme, content = content)
    }
}

fun Palette.of(mode: Mode): Color = when (mode) {
    Mode.READ_ONLY -> green
    Mode.READ_WRITE -> yellow
    Mode.FULL -> red
}

fun Mode.label(): String = when (this) {
    Mode.READ_ONLY -> "Read-only"
    Mode.READ_WRITE -> "Read-write"
    Mode.FULL -> "Full"
}

fun Mode.short(): String = when (this) {
    Mode.READ_ONLY -> "RO"
    Mode.READ_WRITE -> "RW"
    Mode.FULL -> "FULL"
}

fun Engine.label(): String = when (this) {
    Engine.POSTGRES -> "PostgreSQL"
    Engine.MY_SQL -> "MySQL"
    Engine.MONGO -> "MongoDB"
}

fun Engine.badge(): String = when (this) {
    Engine.POSTGRES -> "PG"
    Engine.MY_SQL -> "MY"
    Engine.MONGO -> "MG"
}

fun Palette.of(engine: Engine): Color = when (engine) {
    Engine.POSTGRES -> blue
    Engine.MY_SQL -> orange
    Engine.MONGO -> green
}
