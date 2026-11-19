package dev.dbdelve.android.data

import org.json.JSONObject
import uniffi.dbdelve_core.ConnectionConfig
import uniffi.dbdelve_core.Engine
import uniffi.dbdelve_core.Mode
import uniffi.dbdelve_core.SslMode

/** A saved connection. The password is never here: it lives in [Secrets]. */
data class Profile(
    val id: String,
    val name: String,
    val project: String,
    val engine: Engine,
    val host: String,
    val port: Int,
    val user: String,
    val database: String,
    val sslMode: SslMode,
    val mode: Mode,
) {
    fun config(password: String) = ConnectionConfig(
        engine = engine,
        host = host,
        port = port.toUShort(),
        user = user,
        password = password,
        database = database,
        sslMode = sslMode,
    )

    fun toJson(): JSONObject = JSONObject()
        .put("id", id)
        .put("name", name)
        .put("project", project)
        .put("engine", engine.name)
        .put("host", host)
        .put("port", port)
        .put("user", user)
        .put("database", database)
        .put("ssl_mode", sslMode.name)
        .put("mode", mode.name)

    companion object {
        /**
         * A mode or TLS setting this build does not know reads as the safest
         * one, never as a quietly weaker one.
         */
        fun fromJson(json: JSONObject) = Profile(
            id = json.getString("id"),
            name = json.getString("name"),
            project = json.optString("project"),
            engine = Engine.valueOf(json.getString("engine")),
            host = json.getString("host"),
            port = json.getInt("port"),
            user = json.optString("user"),
            database = json.optString("database"),
            sslMode = runCatching { SslMode.valueOf(json.getString("ssl_mode")) }
                .getOrDefault(SslMode.VERIFY_FULL),
            mode = runCatching { Mode.valueOf(json.getString("mode")) }
                .getOrDefault(Mode.READ_ONLY),
        )
    }
}
