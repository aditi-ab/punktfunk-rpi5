package io.unom.punktfunk.kit.security

import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Test

class IdentityStoreTest {
    private val identity = ClientIdentity("cert", "key")

    @Test
    fun firstMintFailureRetriesInsideTheSameCall() {
        var mints = 0

        val result = obtainAbsentIdentity(load = { IdentityLoad.Absent }, mint = {
            mints++
            if (mints == 1) throw IllegalStateException("transient KeyMint failure")
            identity
        })

        assertSame(identity, result)
        assertEquals(2, mints)
    }

    @Test
    fun mintFailureReReadsARecoveredIdentity() {
        var loads = 0
        var mints = 0

        val result = obtainAbsentIdentity(load = {
            loads++
            if (loads == 1) IdentityLoad.Absent else IdentityLoad.Ok(identity)
        }, mint = {
            mints++
            throw IllegalStateException("late persist result")
        })

        assertSame(identity, result)
        assertEquals(2, loads)
        assertEquals(1, mints)
    }

    @Test
    fun repeatedMintFailureStopsAfterThreeAttempts() {
        var mints = 0
        val failure = IllegalStateException("KeyMint unavailable")

        val thrown = assertThrows(IllegalStateException::class.java) {
            obtainAbsentIdentity(load = { IdentityLoad.Absent }, mint = {
                mints++
                throw failure
            })
        }

        assertSame(failure, thrown)
        assertEquals(3, mints)
    }

    @Test
    fun unrecoverableStoreNeverMints() {
        var mints = 0

        val thrown = assertThrows(IdentityUnrecoverableException::class.java) {
            obtainAbsentIdentity(
                load = { IdentityLoad.Unrecoverable("identity blob is unreadable", null) },
                mint = { mints++; identity },
            )
        }

        assertEquals("identity blob is unreadable", thrown.message)
        assertEquals(0, mints)
    }
}
