import express from 'express';
import ical from 'ical-generator';
import { pool } from '../config/database';
import { authenticateToken, AuthRequest } from '../middleware/auth';

const router = express.Router();

// Generate ICS file for a booking
router.get('/booking/:bookingId/ics', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      `SELECT b.*, vs.start_time, vs.end_time, p.title as property_title, p.address as property_address,
              u.first_name as buyer_first_name, u.last_name as buyer_last_name, u.email as buyer_email
       FROM bookings b
       JOIN viewing_slots vs ON b.viewing_slot_id = vs.id
       JOIN properties p ON vs.property_id = p.id
       JOIN users u ON b.buyer_id = u.id
       WHERE b.id = $1 AND (b.buyer_id = $2 OR EXISTS (
         SELECT 1 FROM properties p2 WHERE p2.id = vs.property_id AND p2.seller_id = $2
       ))`,
      [req.params.bookingId, req.user!.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Booking not found or access denied' });
    }

    const booking = result.rows[0];

    const calendar = ical({
      name: 'Property Viewing',
      description: 'Property viewing appointment'
    });

    calendar.createEvent({
      start: new Date(booking.start_time),
      end: new Date(booking.end_time),
      summary: `Property Viewing: ${booking.property_title}`,
      description: `Property viewing appointment for ${booking.property_title}\n\nAddress: ${booking.property_address}\n\nNotes: ${booking.notes || 'No additional notes'}`,
      location: booking.property_address,
      organizer: {
        name: 'Property Booking System',
        email: 'noreply@propertybooking.com'
      },
      attendees: [
        {
          name: `${booking.buyer_first_name} ${booking.buyer_last_name}`,
          email: booking.buyer_email,
          rsvp: true
        }
      ]
    });

    res.setHeader('Content-Type', 'text/calendar');
    res.setHeader('Content-Disposition', `attachment; filename="property-viewing-${booking.id}.ics"`);
    res.send(calendar.toString());
  } catch (error) {
    next(error);
  }
});

// Generate Gmail calendar link
router.get('/booking/:bookingId/gmail', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      `SELECT b.*, vs.start_time, vs.end_time, p.title as property_title, p.address as property_address
       FROM bookings b
       JOIN viewing_slots vs ON b.viewing_slot_id = vs.id
       JOIN properties p ON vs.property_id = p.id
       WHERE b.id = $1 AND (b.buyer_id = $2 OR EXISTS (
         SELECT 1 FROM properties p2 WHERE p2.id = vs.property_id AND p2.seller_id = $2
       ))`,
      [req.params.bookingId, req.user!.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Booking not found or access denied' });
    }

    const booking = result.rows[0];
    const startTime = new Date(booking.start_time);
    const endTime = new Date(booking.end_time);

    // Format dates for Google Calendar
    const formatDate = (date: Date) => {
      return date.toISOString().replace(/[-:]/g, '').split('.')[0] + 'Z';
    };

    const googleCalendarUrl = new URL('https://calendar.google.com/calendar/render');
    googleCalendarUrl.searchParams.set('action', 'TEMPLATE');
    googleCalendarUrl.searchParams.set('text', `Property Viewing: ${booking.property_title}`);
    googleCalendarUrl.searchParams.set('dates', `${formatDate(startTime)}/${formatDate(endTime)}`);
    googleCalendarUrl.searchParams.set('details', `Property viewing appointment for ${booking.property_title}\n\nAddress: ${booking.property_address}\n\nNotes: ${booking.notes || 'No additional notes'}`);
    googleCalendarUrl.searchParams.set('location', booking.property_address);

    res.json({ url: googleCalendarUrl.toString() });
  } catch (error) {
    next(error);
  }
});

// Generate Outlook calendar link
router.get('/booking/:bookingId/outlook', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      `SELECT b.*, vs.start_time, vs.end_time, p.title as property_title, p.address as property_address
       FROM bookings b
       JOIN viewing_slots vs ON b.viewing_slot_id = vs.id
       JOIN properties p ON vs.property_id = p.id
       WHERE b.id = $1 AND (b.buyer_id = $2 OR EXISTS (
         SELECT 1 FROM properties p2 WHERE p2.id = vs.property_id AND p2.seller_id = $2
       ))`,
      [req.params.bookingId, req.user!.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Booking not found or access denied' });
    }

    const booking = result.rows[0];
    const startTime = new Date(booking.start_time);
    const endTime = new Date(booking.end_time);

    const outlookUrl = new URL('https://outlook.live.com/calendar/0/deeplink/compose');
    outlookUrl.searchParams.set('subject', `Property Viewing: ${booking.property_title}`);
    outlookUrl.searchParams.set('startdt', startTime.toISOString());
    outlookUrl.searchParams.set('enddt', endTime.toISOString());
    outlookUrl.searchParams.set('body', `Property viewing appointment for ${booking.property_title}\n\nAddress: ${booking.property_address}\n\nNotes: ${booking.notes || 'No additional notes'}`);
    outlookUrl.searchParams.set('location', booking.property_address);

    res.json({ url: outlookUrl.toString() });
  } catch (error) {
    next(error);
  }
});

export { router as calendarRoutes };