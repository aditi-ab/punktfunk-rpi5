package io.unom.punktfunk

import io.unom.punktfunk.models.ActiveSession
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** [SessionOwner]: a recreated activity's screen attaches again, and the handle closes once. */
class SessionOwnerTest {
    private val closed = mutableListOf<Long>()
    private val owner = SessionOwner { closed += it }

    private fun open(handle: Long) = owner.open(ActiveSession(handle, Settings(), clipboardSync = false))

    @Test
    fun aRecreatedScreenAttachesAgainAndTheEndClosesOnce() {
        open(7)
        owner.attach(7)
        owner.detach(7) // the old activity's composition goes
        assertEquals(emptyList<Long>(), closed)
        assertEquals(7L, owner.session?.handle)

        owner.attach(7) // the recreated activity's
        owner.end(7)
        owner.end(7)
        assertEquals(emptyList<Long>(), closed) // its planes still run until it detaches
        assertNull(owner.session)

        owner.detach(7)
        assertEquals(listOf(7L), closed)
    }

    @Test
    fun anEndWithNoScreenClosesAtOnce() {
        open(7)
        owner.end(7)
        assertEquals(listOf(7L), closed)
    }

    @Test
    fun aLeavingScreenClosesOnlyItsOwnEndedSession() {
        open(7)
        owner.attach(7)
        owner.end(7)
        open(8) // the next stream comes in while the last one's screen fades out
        owner.attach(8)
        owner.detach(7)
        assertEquals(listOf(7L), closed)

        owner.end(8)
        owner.detach(8)
        assertEquals(listOf(7L, 8L), closed)
    }

    @Test
    fun aStaleEndLeavesTheOpenSession() {
        open(8)
        owner.end(7)
        assertEquals(8L, owner.session?.handle)
        assertEquals(emptyList<Long>(), closed)
    }

    @Test
    fun twoScreensOnOneHandleCloseAfterTheLast() {
        open(7)
        owner.attach(7)
        owner.attach(7) // a second activity instance shows the same session
        owner.end(7)
        owner.detach(7)
        assertEquals(emptyList<Long>(), closed)
        owner.detach(7)
        assertEquals(listOf(7L), closed)
    }
}
