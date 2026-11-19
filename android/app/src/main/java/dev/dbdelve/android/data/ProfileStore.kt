package dev.dbdelve.android.data

import java.io.File
import org.json.JSONArray

/**
 * `profiles.json` in the app's own files. A file that will not load is moved
 * aside to `profiles.json.broken` before anything can save over it.
 */
class ProfileStore(private val directory: File) {
    private val file get() = File(directory, "profiles.json")

    fun load(): List<Profile> {
        if (!file.exists()) return emptyList()
        return try {
            val array = JSONArray(file.readText())
            List(array.length()) { Profile.fromJson(array.getJSONObject(it)) }
        } catch (error: Exception) {
            file.renameTo(File(directory, "profiles.json.broken"))
            emptyList()
        }
    }

    fun save(profiles: List<Profile>) {
        val array = JSONArray()
        profiles.forEach { array.put(it.toJson()) }
        // Written beside and renamed over, so a crash mid-write never leaves
        // half a file to be moved aside.
        val next = File(directory, "profiles.json.next")
        next.writeText(array.toString(2))
        next.renameTo(file)
    }
}
