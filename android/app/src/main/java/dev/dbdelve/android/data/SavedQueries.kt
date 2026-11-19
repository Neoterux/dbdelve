package dev.dbdelve.android.data

import java.io.File
import java.util.UUID
import org.json.JSONArray
import org.json.JSONObject

/** A query kept by name for one connection; `:name` placeholders are its parameters. */
data class SavedQuery(val id: String, val profileId: String, val name: String, val text: String)

/** `saved_queries.json` in the app's own files, every connection's in one list. */
class SavedQueryStore(private val directory: File) {
    private val file get() = File(directory, "saved_queries.json")

    fun load(): List<SavedQuery> = try {
        val array = JSONArray(file.readText())
        List(array.length()) {
            val json = array.getJSONObject(it)
            SavedQuery(json.getString("id"), json.getString("profile"), json.getString("name"), json.getString("text"))
        }
    } catch (missingOrUnreadable: Exception) {
        if (file.exists()) file.renameTo(File(directory, "saved_queries.json.broken"))
        emptyList()
    }

    fun save(queries: List<SavedQuery>) {
        val array = JSONArray()
        queries.forEach {
            array.put(JSONObject().put("id", it.id).put("profile", it.profileId).put("name", it.name).put("text", it.text))
        }
        val next = File(directory, "saved_queries.json.next")
        next.writeText(array.toString(2))
        next.renameTo(file)
    }

    companion object {
        fun newId(): String = UUID.randomUUID().toString()
    }
}
