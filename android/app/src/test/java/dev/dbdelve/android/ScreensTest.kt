package dev.dbdelve.android

import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Looper
import android.view.View
import android.view.WindowManager
import androidx.compose.material3.Surface
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import dev.dbdelve.android.data.Profile
import dev.dbdelve.android.data.ProfileStore
import dev.dbdelve.android.data.SavedQueryStore
import dev.dbdelve.android.data.Secrets
import dev.dbdelve.android.data.TabStore
import dev.dbdelve.android.ui.ConnectionsScreen
import dev.dbdelve.android.ui.DBDelveTheme
import dev.dbdelve.android.ui.WorkspaceScreen
import java.io.File
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import uniffi.dbdelve_core.Engine
import uniffi.dbdelve_core.FilterOp
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.SslMode

/**
 * Draws the app's screens, as a phone would, into build/screens: the real
 * composables over the real core, connected to a real Postgres. Skipped
 * unless `DBDELVE_SCREENS_PG` names one as `host:port/database/user`, with
 * a blank password, and the core is built for the host.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w412dp-h915dp-night-xxhdpi")
class ScreensTest {
    @get:Rule
    val compose = createComposeRule()

    private val out = File(System.getProperty("dbdelve.screens") ?: "build/screens").apply { mkdirs() }

    @Test
    fun screens() {
        val target = System.getenv("DBDELVE_SCREENS_PG")
        assumeTrue("DBDELVE_SCREENS_PG is not set", target != null)
        val (address, database, user) = target!!.split('/')
        val (host, port) = address.split(':')

        val app = RuntimeEnvironment.getApplication()
        val files = File(app.filesDir, "screens-${System.nanoTime()}").apply { mkdirs() }
        val model = AppViewModel(ProfileStore(files), Secrets(app), TabStore(File(files, "tabs")), SavedQueryStore(files)) {}
        val shop = Profile("shop", "Shop (local)", "Demo", Engine.POSTGRES, host, port.toInt(), user, database, SslMode.DISABLE, Mode.READ_ONLY)
        model.saveProfile(shop, null)
        model.saveProfile(shop.copy(id = "reports", name = "Reports replica", project = "Demo", mode = Mode.READ_WRITE), null)
        model.saveProfile(shop.copy(id = "events", name = "Events", project = "Analytics", engine = Engine.MONGO, port = 27017), null)

        compose.setContent {
            DBDelveTheme {
                Surface {
                    when (model.screen) {
                        Screen.Workspace -> WorkspaceScreen(model) { _, then -> then() }
                        else -> ConnectionsScreen(model)
                    }
                }
            }
        }
        shot("01-connections")

        compose.runOnIdle { model.connect(shop) }
        await { model.connectError != null || (model.session != null && model.catalog.isNotEmpty()) }
        check(model.connectError == null) { "connect failed: ${model.connectError}" }
        compose.runOnIdle {
            model.edit(model.tab!!, textAt("SELECT status, count(*), sum(total)\nFROM orders\nGROUP BY status\nORDER BY 2 DESC;"))
            model.newTab("customers", "SELECT * FROM customers WHERE country = 'EC';")
            model.newTab("Query 3", "")
            model.select(0)
        }
        shot("02-editor")

        compose.onNodeWithContentDescription("3 open tabs").performClick()
        shot("03-tab-switcher")
        compose.runOnIdle {
            model.select(1)
            model.edit(model.tab!!, textAt(model.tab!!.text.text))
            model.runAtCursor()
        }
        await { model.tab?.result != null || model.tab?.error != null }
        shot("04-results")

        compose.runOnIdle { model.pane = Pane.SCHEMA }
        shot("05-schema")

        val orders = model.catalog.first { it.name == "orders" }
        compose.runOnIdle { model.build(orders) }
        await { !model.builder.loadingColumns }
        compose.runOnIdle {
            val builder = model.builder
            builder.chosen.addAll(listOf("id", "status", "total"))
            builder.filters.add(BuilderFilter("status").apply { value = "paid" })
            builder.filters.add(BuilderFilter("total").apply { op = FilterOp.GREATER_OR_EQUAL; value = "250" })
            builder.orderBy = "total"
            builder.descending = true
            builder.limit = 100
        }
        shot("06-builder")

        compose.runOnIdle { check(model.openBuilt(run = true) == null) }
        await { model.tab?.result != null || model.tab?.error != null }
        check(model.tab?.error == null) { "the built query failed: ${model.tab?.error}" }
        shot("07-builder-results")

        compose.runOnIdle { model.pane = Pane.QUERY }
        shot("08-built-tab")
    }

    /**
     * Waits for work the view model finishes on a background thread. The test
     * runs on Robolectric's main thread, so what that work posts back runs
     * only when the main looper is drained here.
     */
    private fun await(condition: () -> Boolean) {
        val until = System.currentTimeMillis() + 20_000
        while (!condition()) {
            check(System.currentTimeMillis() < until) { "timed out" }
            shadowOf(Looper.getMainLooper()).idle()
            Thread.sleep(20)
        }
        compose.waitForIdle()
    }

    private fun textAt(text: String) =
        androidx.compose.ui.text.input.TextFieldValue(text, androidx.compose.ui.text.TextRange(0))

    /**
     * Every open window drawn in order onto one picture, as the phone shows
     * it: a sheet is a window of its own above the screen. (PixelCopy, which
     * `captureToImage` waits on, never answers under Robolectric.)
     */
    private fun shot(name: String) {
        compose.waitForIdle()
        compose.runOnIdle {
            val global = Class.forName("android.view.WindowManagerGlobal")
            val instance = global.getMethod("getInstance").invoke(null)
            @Suppress("UNCHECKED_CAST")
            val windows = (global.getDeclaredField("mViews").apply { isAccessible = true }.get(instance) as List<View>)
                .filter { it.isAttachedToWindow && it.visibility == View.VISIBLE && it.width > 0 }
            val screen = windows.first()
            val image = Bitmap.createBitmap(screen.width, screen.height, Bitmap.Config.ARGB_8888)
            val canvas = Canvas(image)
            windows.forEach { window ->
                val params = window.layoutParams as? WindowManager.LayoutParams
                if (window !== screen && params != null && params.flags and WindowManager.LayoutParams.FLAG_DIM_BEHIND != 0) {
                    canvas.drawColor(android.graphics.Color.argb((params.dimAmount * 255).toInt(), 0, 0, 0))
                }
                val at = IntArray(2).also { window.getLocationOnScreen(it) }
                canvas.save()
                canvas.translate(at[0].toFloat(), at[1].toFloat())
                window.draw(canvas)
                canvas.restore()
            }
            File(out, "$name.png").outputStream().use { image.compress(Bitmap.CompressFormat.PNG, 100, it) }
        }
    }
}
