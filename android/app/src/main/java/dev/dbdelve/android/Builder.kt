package dev.dbdelve.android

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import uniffi.dbdelve_core.Binding
import uniffi.dbdelve_core.BuiltQuery
import uniffi.dbdelve_core.CatalogEntry
import uniffi.dbdelve_core.Engine
import uniffi.dbdelve_core.FilterOp
import uniffi.dbdelve_core.ParamValue
import uniffi.dbdelve_core.QuerySpec
import uniffi.dbdelve_core.buildQuery
import uniffi.dbdelve_core.Column as DbColumn
import uniffi.dbdelve_core.Filter as DbFilter

/** One filter row: a column, how it compares, and the value typed for it. */
class BuilderFilter(column: String) {
    var column by mutableStateOf(column)
    var op by mutableStateOf(FilterOp.EQUALS)
    var value by mutableStateOf("")

    /** Null until the user picks: then a value that reads as a number is one. */
    var asNumber by mutableStateOf<Boolean?>(null)

    val takesValue: Boolean get() = op != FilterOp.IS_NULL && op != FilterOp.NOT_NULL

    /** A pattern is always text; otherwise the user's pick, or what the value looks like. */
    val isNumber: Boolean get() = op != FilterOp.LIKE && (asNumber ?: NUMBER.matches(value.trim()))

    private companion object {
        val NUMBER = Regex("""-?\d+(\.\d+)?([eE][+-]?\d+)?""")
    }
}

/**
 * The query builder's choices. The statement itself is written by the core
 * (`buildQuery`), with every typed value as a `:name` parameter filled in by
 * `fillParameters`: nothing typed here is spliced into SQL by the app.
 */
class BuilderState(val table: CatalogEntry? = null) {
    var columns by mutableStateOf<List<DbColumn>>(emptyList())
    var loadingColumns by mutableStateOf(table != null)
    var columnsError by mutableStateOf<String?>(null)

    /** Empty means every column. */
    val chosen = mutableStateListOf<String>()
    val filters = mutableStateListOf<BuilderFilter>()
    var orderBy by mutableStateOf<String?>(null)
    var descending by mutableStateOf(false)
    var limit by mutableStateOf<Int?>(null)

    fun built(engine: Engine): BuiltQuery? {
        val table = table ?: return null
        return buildQuery(
            engine,
            QuerySpec(
                schema = table.schema,
                table = table.name,
                columns = chosen.toList(),
                filters = filters.map { DbFilter(it.column, it.op) },
                orderBy = orderBy,
                descending = descending,
                limit = limit?.toUInt(),
            ),
        )
    }

    /** Each filter's value under the parameter the core named for it. */
    fun bindings(built: BuiltQuery): List<Binding> =
        filters.zip(built.parameters).filter { (filter, name) -> filter.takesValue && name.isNotEmpty() }.map { (filter, name) ->
            Binding(
                name,
                if (filter.isNumber) ParamValue.Number(filter.value) else ParamValue.Text(filter.value),
            )
        }
}
