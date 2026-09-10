import React, { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { useForm } from 'react-hook-form';
import { toast } from 'react-toastify';
import { useAuth } from '../contexts/AuthContext';

interface RegisterForm {
  email: string;
  password: string;
  confirmPassword: string;
  firstName: string;
  lastName: string;
  phone?: string;
  role: 'buyer' | 'seller';
}

export const Register: React.FC = () => {
  const { register: registerUser } = useAuth();
  const navigate = useNavigate();
  const [loading, setLoading] = useState(false);

  const { register, handleSubmit, formState: { errors }, watch } = useForm<RegisterForm>();
  const password = watch('password');

  const onSubmit = async (data: RegisterForm) => {
    setLoading(true);
    try {
      const { confirmPassword, ...userData } = data;
      await registerUser(userData);
      toast.success('Registration successful!');
      navigate('/dashboard');
    } catch (error: any) {
      toast.error(error.response?.data?.error || 'Registration failed');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="max-w-lg mx-auto">
      <div className="card-datum p-8">
        <div className="mb-8">
          <h2 className="text-2xl font-semibold text-midnight-fjord tracking-tight mb-1">
            Create an account
          </h2>
          <p className="text-sm text-dark-utility-3">
            Join PropertyBook as a buyer or seller.
          </p>
        </div>

        <form onSubmit={handleSubmit(onSubmit)} className="space-y-5">
          <div className="grid grid-cols-2 gap-4">
            <div>
              <label className="field-label">First name</label>
              <input
                type="text"
                {...register('firstName', { required: 'Required' })}
                className="field-input"
                placeholder="Jane"
              />
              {errors.firstName && (
                <p className="field-error">{errors.firstName.message}</p>
              )}
            </div>
            <div>
              <label className="field-label">Last name</label>
              <input
                type="text"
                {...register('lastName', { required: 'Required' })}
                className="field-input"
                placeholder="Smith"
              />
              {errors.lastName && (
                <p className="field-error">{errors.lastName.message}</p>
              )}
            </div>
          </div>

          <div>
            <label className="field-label">Email address</label>
            <input
              type="email"
              {...register('email', { required: 'Email is required' })}
              className="field-input"
              placeholder="you@example.com"
            />
            {errors.email && (
              <p className="field-error">{errors.email.message}</p>
            )}
          </div>

          <div>
            <label className="field-label">
              Phone <span className="text-dark-utility-4 font-normal">(optional)</span>
            </label>
            <input
              type="tel"
              {...register('phone')}
              className="field-input"
              placeholder="+1 555 000 0000"
            />
          </div>

          <div>
            <label className="field-label">I am a…</label>
            <select
              {...register('role', { required: 'Please select a role' })}
              className="field-select"
            >
              <option value="">Select role</option>
              <option value="buyer">Buyer</option>
              <option value="seller">Seller</option>
            </select>
            {errors.role && (
              <p className="field-error">{errors.role.message}</p>
            )}
          </div>

          {/* Divider */}
          <div className="border-t border-glacier-mist-900 pt-1" />

          <div>
            <label className="field-label">Password</label>
            <input
              type="password"
              {...register('password', {
                required: 'Password is required',
                minLength: { value: 6, message: 'Minimum 6 characters' },
              })}
              className="field-input"
              placeholder="••••••••"
            />
            {errors.password && (
              <p className="field-error">{errors.password.message}</p>
            )}
          </div>

          <div>
            <label className="field-label">Confirm password</label>
            <input
              type="password"
              {...register('confirmPassword', {
                required: 'Please confirm your password',
                validate: value => value === password || 'Passwords do not match',
              })}
              className="field-input"
              placeholder="••••••••"
            />
            {errors.confirmPassword && (
              <p className="field-error">{errors.confirmPassword.message}</p>
            )}
          </div>

          <button
            type="submit"
            disabled={loading}
            className="btn-datum-primary w-full py-2.5 text-sm disabled:opacity-50"
          >
            {loading ? 'Creating account…' : 'Create account'}
          </button>
        </form>

        <p className="text-center mt-6 text-sm text-dark-utility-3">
          Already have an account?{' '}
          <Link to="/login" className="text-canyon-clay-links hover:underline font-medium">
            Sign in
          </Link>
        </p>
      </div>
    </div>
  );
};
