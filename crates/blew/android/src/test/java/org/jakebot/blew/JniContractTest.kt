package org.jakebot.blew

import org.jakebot.blew.JniTestSupport.descriptorOf
import org.jakebot.blew.JniTestSupport.rustSources
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.lang.reflect.Modifier

/**
 * Asserts that every Kotlin method the Rust backend reaches through
 * `call_static_method` exists, is static, and has exactly the descriptor the
 * Rust passes. [JniNativeContractTest] checks the other direction.
 *
 * Nothing else checks this. cargo and Gradle never read each other's method
 * tables, so a missing `@JvmStatic` -- which publishes no static of that name
 * on a Kotlin `object` -- or a stale `jni_sig!` is a runtime `MethodNotFound`
 * behind green CI. 0.4.0-beta.2 shipped that and aborted on launch.
 *
 * The expected list is derived from the Rust rather than restated here: the
 * `jni_str!`/`jni_sig!` pair at each call site is the declaration, so neither
 * side can change without the other following.
 */
class JniContractTest {
    private data class CallSite(
        val classNames: List<String>,
        val method: String,
        val signature: String,
        val origin: String,
    )

    @Test
    fun `every call_static_method site is parsed`() {
        val declared = rustSources().sumOf { CALL_SITE_MARKER.findAll(it.text).count() }
        assertTrue("found no call_static_method sites to check", declared > 0)
        assertEquals(
            "the call_static_method sites and the parser have diverged; " +
                "update CALL_SITE in this test to match the new call shape",
            declared,
            callSites().size,
        )
    }

    @Test
    fun `JNI-called methods are static with the declared descriptor`() {
        val failures = mutableListOf<String>()

        for (site in callSites()) {
            for (className in site.classNames) {
                val cls = Class.forName(className, false, javaClass.classLoader)
                val matching =
                    cls.declaredMethods.filter {
                        it.name == site.method && descriptorOf(it) == site.signature
                    }
                val where = "$className.${site.method}${site.signature} (${site.origin})"
                when {
                    matching.isEmpty() -> {
                        val overloads =
                            cls.declaredMethods
                                .filter { it.name == site.method }
                                .joinToString(", ") { descriptorOf(it) }
                                .ifEmpty { "<no method by that name>" }
                        failures += "$where: not declared; found $overloads"
                    }

                    matching.none { Modifier.isStatic(it.modifiers) } -> {
                        failures += "$where: declared but not static -- add @JvmStatic"
                    }
                }
            }
        }

        if (failures.isNotEmpty()) {
            fail(failures.joinToString("\n", prefix = "JNI contract violations:\n"))
        }
    }

    private fun callSites(): List<CallSite> =
        rustSources().flatMap { source ->
            CALL_SITE.findAll(source.text).map { match ->
                val (classExpr, method, signature) = match.destructured
                val classNames =
                    CLASS_EXPRESSIONS[classExpr.trim()]
                        ?: throw AssertionError(
                            "${source.path}: don't know which class `${classExpr.trim()}` resolves to; " +
                                "add it to CLASS_EXPRESSIONS in this test",
                        )
                CallSite(classNames, method, signature, source.path)
            }
        }

    private companion object {
        const val CENTRAL = "org.jakebot.blew.BleCentralManager"
        const val PERIPHERAL = "org.jakebot.blew.BlePeripheralManager"
        const val PLUGIN = "org.jakebot.blew.BlewPlugin"

        val CALL_SITE_MARKER = Regex("""call_static_method\(""")

        val CALL_SITE =
            Regex(
                """call_static_method\(\s*([^,]+?)\s*,\s*""" +
                    """jni_str!\("([^"]+)"\)\s*,\s*jni_sig!\("([^"]+)"\)\s*,""",
            )

        /**
         * How each class expression used at a call site resolves. An expression
         * that isn't here fails the test rather than going unchecked, which is
         * why the Rust keeps the class at every site to a single expression.
         */
        val CLASS_EXPRESSIONS =
            mapOf(
                "central_class()" to listOf(CENTRAL),
                "jni_globals::central_class()" to listOf(CENTRAL),
                "peripheral_class()" to listOf(PERIPHERAL),
                "jni_globals::peripheral_class()" to listOf(PERIPHERAL),
                // Picks a role's class at runtime, so both must satisfy the call.
                "socket_class(is_server)" to listOf(CENTRAL, PERIPHERAL),
                "manager_class" to listOf(CENTRAL, PERIPHERAL),
                "&plugin_class" to listOf(PLUGIN),
            )
    }
}
