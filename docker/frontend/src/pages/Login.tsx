import React, { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { useForm } from 'react-hook-form';
import { toast } from 'react-toastify';
import { useAuth } from '../contexts/AuthContext';

interface LoginForm {
  email: string;
  password: string;
}

export const Login: React.FC = () => {
  const { login } = useAuth();
  const navigate = useNavigate();
  const [loading, setLoading] = useState(false);

  const { register, handleSubmit, formState: { errors } } = useForm<LoginForm>();

  const onSubmit = async (data: LoginForm) => {
    setLoading(true);
    try {
      await login(data.email, data.password);
      toast.success('Login successful!');
      navigate('/dashboard');
    } catch (error: any) {
      toast.error(error.response?.data?.error || 'Login failed');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="max-w-md mx-auto">
      <div className="card-datum p-8">
        <div className="mb-8">
          <h2 className="text-2xl font-semibold text-midnight-fjord tracking-tight mb-1">
            Sign in
          </h2>
          <p className="text-sm text-dark-utility-3">
            Welcome back — enter your details below.
          </p>
        </div>

        <form onSubmit={handleSubmit(onSubmit)} className="space-y-5">
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
            <label className="field-label">Password</label>
            <input
              type="password"
              {...register('password', { required: 'Password is required' })}
              className="field-input"
              placeholder="••••••••"
            />
            {errors.password && (
              <p className="field-error">{errors.password.message}</p>
            )}
          </div>

          <button
            type="submit"
            disabled={loading}
            className="btn-datum-primary w-full py-2.5 text-sm disabled:opacity-50"
          >
            {loading ? 'Signing in…' : 'Sign in'}
          </button>
        </form>

        <p className="text-center mt-6 text-sm text-dark-utility-3">
          Don't have an account?{' '}
          <Link to="/register" className="text-canyon-clay-links hover:underline font-medium">
            Create one
          </Link>
        </p>
      </div>
    </div>
  );
};
