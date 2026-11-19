package dev.dbdelve.android.data

import java.io.File
import org.json.JSONArray
import org.json.JSONObject

/**
 * Each connection's query tabs, so a buffer survives Android reclaiming the
 * app. App-private files, kept out of backups like everything else here.
 * Results are not kept: a reconnect runs nothing by itself.
 */
class TabStore(private val directory: File) {
    data class Saved(val title: String, val text: String)

    private fun file(profileId: String) = File(directory, "$profileId.json")

    /** The tabs and which one was open; none for a connection never opened. */
    fun load(profileId: String): Pair<List<Saved>, Int> = try {
        val json = JSONObject(file(profileId).readText())
        val tabs = json.getJSONArray("tabs")
        List(tabs.length()) {
            val tab = tabs.getJSONObject(it)
            Saved(tab.getString("title"), tab.getString("text"))
        } to json.optInt("current")
    } catch (missingOrUnreadable: Exception) {
        // A file that will not read loses only buffers, never connections.
        emptyList<Saved>() to 0
    }

    fun save(profileId: String, tabs: List<Saved>, current: Int) {
        directory.mkdirs()
        val array = JSONArray()
        tabs.forEach { array.put(JSONObject().put("title", it.title).put("text", it.text)) }
        val next = File(directory, "$profileId.json.next")
        next.writeText(JSONObject().put("tabs", array).put("current", current).toString())
        next.renameTo(file(profileId))
    }

    fun forget(profileId: String) {
        file(profileId).delete()
    }
}
