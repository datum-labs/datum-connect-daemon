import express from 'express';
import multer from 'multer';
import path from 'path';
import { v4 as uuidv4 } from 'uuid';
import { pool } from '../config/database';
import { authenticateToken, requireRole, AuthRequest } from '../middleware/auth';

const router = express.Router();

// Configure multer for file uploads
const storage = multer.diskStorage({
  destination: (req, file, cb) => {
    cb(null, 'uploads/documents/');
  },
  filename: (req, file, cb) => {
    const uniqueName = `${uuidv4()}${path.extname(file.originalname)}`;
    cb(null, uniqueName);
  }
});

const upload = multer({
  storage,
  limits: {
    fileSize: 10 * 1024 * 1024 // 10MB limit
  },
  fileFilter: (req, file, cb) => {
    const allowedTypes = ['.pdf', '.jpg', '.jpeg', '.png', '.doc', '.docx'];
    const ext = path.extname(file.originalname).toLowerCase();
    
    if (allowedTypes.includes(ext)) {
      cb(null, true);
    } else {
      cb(new Error('Invalid file type. Only PDF, images, and Word documents are allowed.'));
    }
  }
});

// Upload financial document
router.post('/upload', authenticateToken, upload.single('document'), async (req: AuthRequest, res, next) => {
  try {
    if (!req.file) {
      return res.status(400).json({ error: 'No file uploaded' });
    }

    const { documentType } = req.body;
    
    if (!documentType) {
      return res.status(400).json({ error: 'Document type is required' });
    }

    const result = await pool.query(
      'INSERT INTO financial_documents (user_id, document_type, file_path) VALUES ($1, $2, $3) RETURNING *',
      [req.user!.id, documentType, req.file.path]
    );

    res.status(201).json({
      id: result.rows[0].id,
      documentType: result.rows[0].document_type,
      verificationStatus: result.rows[0].verification_status,
      createdAt: result.rows[0].created_at
    });
  } catch (error) {
    next(error);
  }
});

// Get user's documents
router.get('/my-documents', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      'SELECT id, document_type, verification_status, verified_at, created_at FROM financial_documents WHERE user_id = $1 ORDER BY created_at DESC',
      [req.user!.id]
    );

    res.json(result.rows);
  } catch (error) {
    next(error);
  }
});

// Verify document (admin/agent only)
router.put('/:id/verify', authenticateToken, requireRole(['admin', 'agent']), async (req: AuthRequest, res, next) => {
  try {
    const { status } = req.body;
    
    if (!['verified', 'rejected'].includes(status)) {
      return res.status(400).json({ error: 'Status must be verified or rejected' });
    }

    const result = await pool.query(
      'UPDATE financial_documents SET verification_status = $1, verified_by = $2, verified_at = CURRENT_TIMESTAMP WHERE id = $3 RETURNING *',
      [status, req.user!.id, req.params.id]
    );

    if (result.rows.length === 0) {
      return res.status(404).json({ error: 'Document not found' });
    }

    res.json({
      id: result.rows[0].id,
      verificationStatus: result.rows[0].verification_status,
      verifiedAt: result.rows[0].verified_at
    });
  } catch (error) {
    next(error);
  }
});

// Get pending documents for verification
router.get('/pending', authenticateToken, requireRole(['admin', 'agent']), async (req, res, next) => {
  try {
    const result = await pool.query(
      `SELECT fd.*, u.first_name, u.last_name, u.email 
       FROM financial_documents fd 
       JOIN users u ON fd.user_id = u.id 
       WHERE fd.verification_status = 'pending' 
       ORDER BY fd.created_at ASC`
    );

    res.json(result.rows);
  } catch (error) {
    next(error);
  }
});

// Check if user has verified documents
router.get('/verification-status', authenticateToken, async (req: AuthRequest, res, next) => {
  try {
    const result = await pool.query(
      'SELECT COUNT(*) as verified_count FROM financial_documents WHERE user_id = $1 AND verification_status = $2',
      [req.user!.id, 'verified']
    );

    const hasVerifiedDocuments = parseInt(result.rows[0].verified_count) > 0;

    res.json({
      hasVerifiedDocuments,
      canBookViewings: hasVerifiedDocuments
    });
  } catch (error) {
    next(error);
  }
});

export { router as documentRoutes };