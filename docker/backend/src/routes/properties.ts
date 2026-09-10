import express from 'express';
import Joi from 'joi';
import { pool } from '../config/database';
import { authenticateToken, requireRole, AuthRequest } from '../middleware/auth';

const router = express.Router();

const propertySchema = Joi.object({
  title: Joi.string().required(),
  description: Joi.string().optional(),
  address: Joi.string().required(),
  price: Joi.number().positive().required(),
  propertyType: Joi.string().required(),
  bedrooms: Joi.number().integer().min(0).optional(),
  bathrooms: Joi.number().integer().min(0).optional(),
  squareFeet: Joi.number().integer().min(0).optional()
});

// Get all properties
router.get('/', async (req, res, next) => {
  try {
    const { page = 1, limit = 10, status = 'active' } = req.query;
    const offset = (Number(page) - 1) * Number(limit);

    const result = await pool.query(
      `SELECT p.*, u.first_name as seller_first_name, u.last_name as seller_last_name 
       FROM properties p 
       JOIN users u ON p.seller_id = u.id 
       WHERE p.status = $1 
       ORDER BY p.created_at DESC 
       LIMIT $2 OFFSET $3`,
      [status, limit, offset]
    );

    const countResult = await pool.query(
      'SELECT COUNT(*) FROM properties WHERE status = $1',
      [status]
    );

    res.json({
      properties: result.rows,
      total: parseInt(countResult.rows[0].count),
      page: Number(page),
      limit: Number(limit)
    });
  } catch (error) {
    next(error);
  }
});

// Get property by ID
router.get('/:id', async (req, res, next) => {
  try {
    const result = await pool.query(
      `SELECT p.*, u.first_name as seller_first_name, u.last_name as seller_last_name 
       FROM properties p 
       JOIN users u ON p.seller_id = u.id 
       WHERE p.id = $1`,
      [req.params.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Property not found' });
    }

    res.json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

// Create property
router.post('/', authenticateToken, requireRole(['seller', 'agent']), async (req: AuthRequest, res, next) => {
  try {
    const { error, value } = propertySchema.validate(req.body);
    if (error) {
      return res.status(400).json({ error: error.details[0].message });
    }

    const { title, description, address, price, propertyType, bedrooms, bathrooms, squareFeet } = value;

    const result = await pool.query(
      `INSERT INTO properties (title, description, address, price, property_type, bedrooms, bathrooms, square_feet, seller_id) 
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) 
       RETURNING *`,
      [title, description, address, price, propertyType, bedrooms, bathrooms, squareFeet, req.user!.id]
    );

    res.status(201).json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

// Update property
router.put('/:id', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const { error, value } = propertySchema.validate(req.body);
    if (error) {
      return res.status(400).json({ error: error.details[0].message });
    }

    // Check if user owns the property
    const propertyCheck = await pool.query(
      'SELECT seller_id FROM properties WHERE id = $1',
      [req.params.id]
    );

    if (propertyCheck.rows.length === 0) {
      return res.status(404).json({ error: 'Property not found' });
    }

    if (propertyCheck.rows[0].seller_id !== req.user!.id && req.user!.role !== 'admin') {
      return res.status(403).json({ error: 'Not authorized to update this property' });
    }

    const { title, description, address, price, propertyType, bedrooms, bathrooms, squareFeet } = value;

    const result = await pool.query(
      `UPDATE properties 
       SET title = $1, description = $2, address = $3, price = $4, property_type = $5, 
           bedrooms = $6, bathrooms = $7, square_feet = $8, updated_at = CURRENT_TIMESTAMP
       WHERE id = $9 
       RETURNING *`,
      [title, description, address, price, propertyType, bedrooms, bathrooms, squareFeet, req.params.id]
    );

    res.json(result.rows[0]);
  } catch (error) {
    next(error);
  }
});

export { router as propertyRoutes };