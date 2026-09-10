import express from 'express';
import Joi from 'joi';
import { pool } from '../config/database';
import { authenticateToken, requireRole, AuthRequest } from '../middleware/auth';

const router = express.Router();

const viewingSlotSchema = Joi.object({
  propertyId: Joi.string().uuid().required(),
  startTime: Joi.date().iso().required(),
  endTime: Joi.date().iso().required(),
  maxAttendees: Joi.number().integer().min(1).default(1)
});

const bookingSchema = Joi.object({
  viewingSlotId: Joi.string().uuid().required(),
  notes: Joi.string().optional()
});

// Create viewing slot
router.post('/slots', authenticateToken, requireRole(['seller', 'agent']), async (req: AuthRequest, res, next) => {
  try {
    const { error, value } = viewingSlotSchema.validate(req.body);
    if (error) {
      return res.status(400).json({ error: error.details[0].message });
    }

    const { propertyId, startTime, endTime, maxAttendees } = value;

    // Check if user owns the property
    const propertyCheck = await pool.query(
      'SELECT seller_id FROM properties WHERE id = $1',
      [propertyId]
    );

    if (propertyCheck.rows.length === 0) {
      return res.status(404).json({ error: 'Property not found' });
    }

    if (propertyCheck.rows[0].seller_id !== req.user!.id && req.user!.role !== 'admin') {
      return res.status(403).json({ error: 'Not authorized to create slots for this property' });
    }

    const result = await pool.query(
      'INSERT INTO viewing_slots (property_id, start_time, end_time, max_attendees) VALUES ($1, $2, $3, $4) RETURNING *',
      [propertyId, startTime, endTime, maxAttendees]
    );

    res.status(201).json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

// Get viewing slots for a property
router.get('/slots/property/:propertyId', async (req, res, next) => {
  try {
    const result = await pool.query(
      `SELECT vs.*, 
              (vs.max_attendees - COALESCE(booking_count.count, 0)) as available_spots
       FROM viewing_slots vs
       LEFT JOIN (
         SELECT viewing_slot_id, COUNT(*) as count 
         FROM bookings 
         WHERE status IN ('confirmed', 'pending') 
         GROUP BY viewing_slot_id
       ) booking_count ON vs.id = booking_count.viewing_slot_id
       WHERE vs.property_id = $1 AND vs.start_time > NOW()
       ORDER BY vs.start_time ASC`,
      [req.params.propertyId]
    );

    res.json(result.rows);
  } catch (error) {
    next(error);
  }
});

// Book a viewing slot
router.post('/book', authenticateToken, requireRole(['buyer']), async (req: AuthRequest, res, next) => {
  try {
    const { error, value } = bookingSchema.validate(req.body);
    if (error) {
      return res.status(400).json({ error: error.details[0].message });
    }

    // Check if user has verified documents
    const docCheck = await pool.query(
      'SELECT COUNT(*) as verified_count FROM financial_documents WHERE user_id = $1 AND verification_status = $2',
      [req.user!.id, 'verified']
    );

    if (parseInt(docCheck.rows[0].verified_count) === 0) {
      return res.status(403).json({ error: 'You must have verified financial documents to book viewings' });
    }

    const { viewingSlotId, notes } = value;

    // Check if slot is available
    const slotCheck = await pool.query(
      `SELECT vs.*, 
              (vs.max_attendees - COALESCE(booking_count.count, 0)) as available_spots
       FROM viewing_slots vs
       LEFT JOIN (
         SELECT viewing_slot_id, COUNT(*) as count 
         FROM bookings 
         WHERE status IN ('confirmed', 'pending') 
         GROUP BY viewing_slot_id
       ) booking_count ON vs.id = booking_count.viewing_slot_id
       WHERE vs.id = $1`,
      [viewingSlotId]
    );

    if (slotCheck.rows.length === 0) {
      return res.status(404).json({ error: 'Viewing slot not found' });
    }

    const slot = slotCheck.rows[0];
    if (slot.available_spots <= 0) {
      return res.status(400).json({ error: 'No available spots for this viewing slot' });
    }

    // Check if user already booked this slot
    const existingBooking = await pool.query(
      'SELECT id FROM bookings WHERE viewing_slot_id = $1 AND buyer_id = $2 AND status IN ($3, $4)',
      [viewingSlotId, req.user!.id, 'confirmed', 'pending']
    );

    if (existingBooking.rows.length > 0) {
      return res.status(400).json({ error: 'You have already booked this viewing slot' });
    }

    const result = await pool.query(
      'INSERT INTO bookings (viewing_slot_id, buyer_id, notes) VALUES ($1, $2, $3) RETURNING *',
      [viewingSlotId, req.user!.id, notes]
    );

    res.status(201).json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

// Get user's bookings
router.get('/my-bookings', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      `SELECT b.*, vs.start_time, vs.end_time, p.title as property_title, p.address as property_address
       FROM bookings b
       JOIN viewing_slots vs ON b.viewing_slot_id = vs.id
       JOIN properties p ON vs.property_id = p.id
       WHERE b.buyer_id = $1
       ORDER BY vs.start_time DESC`,
      [req.user!.id]
    );

    res.json(result.rows);
  } catch (error) {
    next(error);
  }
});

// Confirm booking (seller/agent only)
router.put('/:id/confirm', authenticateToken, requireRole(['seller', 'agent']), async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      `UPDATE bookings 
       SET status = 'confirmed', updated_at = CURRENT_TIMESTAMP 
       WHERE id = $1 AND status = 'pending'
       RETURNING *`,
      [req.params.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Booking not found or already processed' });
    }

    res.json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

export { router as bookingRoutes };