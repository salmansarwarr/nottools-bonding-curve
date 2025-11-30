use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_2022::{self as token_2022};
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

declare_id!("CPMWvEXzNTnrksm1PPXQzp2UUTXWxCKQaw9HhvDdf3nT");

#[program]
pub mod bonding_curve {
    use super::*;

    /// Initialize bonding curve with first buyer lock mechanism
    pub fn initialize_bonding_curve(
        ctx: Context<InitializeBondingCurve>,
        initial_virtual_token_reserves: u64,
        initial_virtual_sol_reserves: u64,
        migration_threshold: u64,
        total_supply: u64,
        holder_threshold: u64,
        volume_threshold_usd_cents: u64,
    ) -> Result<()> {
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        bonding_curve.token_mint = ctx.accounts.token_mint.key();
        bonding_curve.creator = ctx.accounts.creator.key();
        bonding_curve.oracle_authority = ctx.accounts.oracle_authority.key();
        bonding_curve.virtual_token_reserves = initial_virtual_token_reserves;
        bonding_curve.virtual_sol_reserves = initial_virtual_sol_reserves;
        bonding_curve.real_token_reserves = total_supply;
        bonding_curve.real_sol_reserves = 0;
        bonding_curve.migration_threshold = migration_threshold;
        bonding_curve.is_migrated = false;
        bonding_curve.total_supply = total_supply;
        bonding_curve.first_buyer = None;
        bonding_curve.first_buyer_locked_amount = 0;
        bonding_curve.first_buyer_lock_active = false;
        bonding_curve.holder_threshold = holder_threshold;
        bonding_curve.volume_threshold = volume_threshold_usd_cents;
        bonding_curve.token_vault_bump = ctx.bumps.token_vault;
        bonding_curve.sol_vault_bump = ctx.bumps.sol_vault;
        bonding_curve.bump = ctx.bumps.bonding_curve;
        bonding_curve.migration_sol = 0;
        bonding_curve.migration_tokens = 0;

        msg!(
            "Bonding curve initialized for mint: {}",
            ctx.accounts.token_mint.key()
        );
        msg!(
            "📊 First buyer lock conditions: {} holders, ${} volume",
            holder_threshold,
            volume_threshold_usd_cents / 100
        );

        Ok(())
    }

    /// Buy function with first buyer detection and locking
    pub fn buy(ctx: Context<Trade>, sol_amount: u64, min_tokens_out: u64) -> Result<()> {
        let bonding_curve_info = ctx.accounts.bonding_curve.to_account_info().clone();
        let bonding_curve = &mut ctx.accounts.bonding_curve;
    
        require!(!bonding_curve.is_migrated, ErrorCode::AlreadyMigrated);
    
        let tokens_out = calculate_tokens_out(
            sol_amount,
            bonding_curve.virtual_sol_reserves + bonding_curve.real_sol_reserves,
            bonding_curve.virtual_token_reserves + bonding_curve.real_token_reserves,
        )?;
    
        require!(tokens_out >= min_tokens_out, ErrorCode::SlippageExceeded);
        require!(
            tokens_out <= bonding_curve.real_token_reserves,
            ErrorCode::InsufficientLiquidity
        );
    
        // Check if this is the first buy
        let is_first_buy = bonding_curve.first_buyer.is_none();
    
        // Transfer SOL from buyer to bonding curve vault
        let cpi_context = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.buyer.to_account_info(),
                to: ctx.accounts.bonding_curve_sol_vault.to_account_info(),
            },
        );
        anchor_lang::system_program::transfer(cpi_context, sol_amount)?;
    
        // Prepare PDA seeds for token transfer
        let seeds = &[
            b"bonding_curve",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.bump],
        ];
        let signer = &[&seeds[..]];
    
        if is_first_buy {
            // 🔒 FIRST BUY - Lock 60% of tokens, send 40% to buyer
            let locked_amount = tokens_out
                .checked_mul(60)
                .ok_or(ErrorCode::MathOverflow)?
                .checked_div(100)
                .ok_or(ErrorCode::MathOverflow)?;
            
            let immediate_amount = tokens_out
                .checked_sub(locked_amount)
                .ok_or(ErrorCode::MathOverflow)?;
    
            msg!(
                "🔒 FIRST BUY DETECTED - Locking {} tokens (60%), sending {} tokens (40%) to buyer {}",
                locked_amount,
                immediate_amount,
                ctx.accounts.buyer.key()
            );
    
            // Transfer 60% to lock vault
            let cpi_accounts_lock = token_2022::TransferChecked {
                from: ctx.accounts.bonding_curve_token_vault.to_account_info(),
                mint: ctx.accounts.token_mint.to_account_info(),
                to: ctx.accounts.first_buyer_lock_vault.to_account_info(),
                authority: bonding_curve_info.clone(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program.clone(), cpi_accounts_lock, signer);
            token_2022::transfer_checked(cpi_ctx, locked_amount, 9)?;
    
            // Transfer 40% to buyer immediately
            let cpi_accounts_buyer = token_2022::TransferChecked {
                from: ctx.accounts.bonding_curve_token_vault.to_account_info(),
                mint: ctx.accounts.token_mint.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: bonding_curve_info,
            };
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts_buyer, signer);
            token_2022::transfer_checked(cpi_ctx, immediate_amount, 9)?;
    
            // Update bonding curve state
            bonding_curve.first_buyer = Some(ctx.accounts.buyer.key());
            bonding_curve.first_buyer_locked_amount = locked_amount;
            bonding_curve.first_buyer_lock_active = true;
    
            emit!(FirstBuyerLocked {
                token_mint: bonding_curve.token_mint,
                buyer: ctx.accounts.buyer.key(),
                amount: locked_amount,
                sol_spent: sol_amount,
                holder_threshold: bonding_curve.holder_threshold,
                volume_threshold: bonding_curve.volume_threshold,
                timestamp: Clock::get()?.unix_timestamp,
            });
        } else {
            // Normal buy - transfer to buyer's token account
            let cpi_accounts = token_2022::TransferChecked {
                from: ctx.accounts.bonding_curve_token_vault.to_account_info(),
                mint: ctx.accounts.token_mint.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: bonding_curve_info,
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            token_2022::transfer_checked(cpi_ctx, tokens_out, 9)?;
        }
    
        // Update reserves
        bonding_curve.real_sol_reserves = bonding_curve
            .real_sol_reserves
            .checked_add(sol_amount)
            .ok_or(ErrorCode::MathOverflow)?;
    
        bonding_curve.real_token_reserves = bonding_curve
            .real_token_reserves
            .checked_sub(tokens_out)
            .ok_or(ErrorCode::MathOverflow)?;
    
        msg!("Buy: {} SOL for {} tokens", sol_amount, tokens_out);
    
        // Check if migration threshold is reached
        if bonding_curve.real_sol_reserves >= bonding_curve.migration_threshold {
            msg!("🚀 Migration threshold reached! Ready for Raydium migration.");
            emit!(MigrationThresholdReached {
                token_mint: bonding_curve.token_mint,
                sol_reserves: bonding_curve.real_sol_reserves,
                timestamp: Clock::get()?.unix_timestamp,
            });
        }
    
        Ok(())
    }

    pub fn sell(ctx: Context<Trade>, token_amount: u64, min_sol_out: u64) -> Result<()> {
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        require!(!bonding_curve.is_migrated, ErrorCode::AlreadyMigrated);

        let sol_out = calculate_sol_out(
            token_amount,
            bonding_curve.virtual_token_reserves + bonding_curve.real_token_reserves,
            bonding_curve.virtual_sol_reserves + bonding_curve.real_sol_reserves,
        )?;

        require!(sol_out >= min_sol_out, ErrorCode::SlippageExceeded);
        require!(
            sol_out <= bonding_curve.real_sol_reserves,
            ErrorCode::InsufficientLiquidity
        );

        // Transfer tokens from seller to bonding curve vault
        let cpi_accounts = token_2022::TransferChecked {
            from: ctx.accounts.buyer_token_account.to_account_info(),
            mint: ctx.accounts.token_mint.to_account_info(),
            to: ctx.accounts.bonding_curve_token_vault.to_account_info(),
            authority: ctx.accounts.buyer.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token_2022::transfer_checked(cpi_ctx, token_amount, 9)?;

        // Transfer SOL from bonding curve vault to seller
        let sol_vault_seeds: &[&[u8]] = &[
            b"sol_vault",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.sol_vault_bump],
        ];

        let transfer_ix = anchor_lang::solana_program::system_instruction::transfer(
            &ctx.accounts.bonding_curve_sol_vault.key(),
            &ctx.accounts.buyer.key(),
            sol_out,
        );

        anchor_lang::solana_program::program::invoke_signed(
            &transfer_ix,
            &[
                ctx.accounts.bonding_curve_sol_vault.to_account_info(),
                ctx.accounts.buyer.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[sol_vault_seeds],
        )?;

        // Update reserves
        bonding_curve.real_sol_reserves = bonding_curve
            .real_sol_reserves
            .checked_sub(sol_out)
            .ok_or(ErrorCode::MathOverflow)?;

        bonding_curve.real_token_reserves = bonding_curve
            .real_token_reserves
            .checked_add(token_amount)
            .ok_or(ErrorCode::MathOverflow)?;

        msg!("Sell: {} tokens for {} SOL", token_amount, sol_out);

        Ok(())
    }

    pub fn migrate_to_raydium(ctx: Context<MigrateToRaydium>) -> Result<()> {
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        require!(!bonding_curve.is_migrated, ErrorCode::AlreadyMigrated);

        let threshold_with_tolerance = bonding_curve
            .migration_threshold
            .checked_mul(999)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(1000)
            .ok_or(ErrorCode::MathOverflow)?;

        require!(
            bonding_curve.real_sol_reserves >= threshold_with_tolerance,
            ErrorCode::ThresholdNotReached
        );

        let total_sol = bonding_curve.real_sol_reserves;
        let platform_fee = total_sol
            .checked_mul(3)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(100)
            .ok_or(ErrorCode::MathOverflow)?;

        let liquidity_sol = total_sol
            .checked_sub(platform_fee)
            .ok_or(ErrorCode::MathOverflow)?;

        let liquidity_tokens = bonding_curve.real_token_reserves;

        // Transfer platform fee
        let sol_vault_seeds: &[&[u8]] = &[
            b"sol_vault",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.sol_vault_bump],
        ];

        let transfer_ix = anchor_lang::solana_program::system_instruction::transfer(
            &ctx.accounts.bonding_curve_sol_vault.key(),
            &ctx.accounts.platform_authority.key(),
            platform_fee,
        );

        anchor_lang::solana_program::program::invoke_signed(
            &transfer_ix,
            &[
                ctx.accounts.bonding_curve_sol_vault.to_account_info(),
                ctx.accounts.platform_authority.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[sol_vault_seeds],
        )?;

        msg!("✅ Migration prepared!");
        msg!("Platform fee: {} lamports", platform_fee);
        msg!("Liquidity SOL: {} lamports", liquidity_sol);
        msg!("Liquidity tokens: {}", liquidity_tokens);

        bonding_curve.is_migrated = true;
        bonding_curve.migration_sol = liquidity_sol;
        bonding_curve.migration_tokens = liquidity_tokens;

        emit!(MigrationCompleted {
            token_mint: bonding_curve.token_mint,
            liquidity_sol,
            liquidity_tokens,
            platform_fee,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn withdraw_for_pool(ctx: Context<WithdrawForPool>) -> Result<()> {
        let bonding_curve = &ctx.accounts.bonding_curve;

        require!(bonding_curve.is_migrated, ErrorCode::NotMigrated);
        require!(
            bonding_curve.migration_tokens > 0,
            ErrorCode::AlreadyWithdrawn
        );

        let seeds = &[
            b"bonding_curve",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.bump],
        ];
        let signer = &[&seeds[..]];

        let vault_balance = ctx.accounts.bonding_curve_token_vault.amount;
        let transfer_amount = std::cmp::min(vault_balance, bonding_curve.migration_tokens);

        require!(transfer_amount > 0, ErrorCode::InsufficientLiquidity);

        // Transfer tokens to destination
        let cpi_accounts = token_2022::TransferChecked {
            from: ctx.accounts.bonding_curve_token_vault.to_account_info(),
            mint: ctx.accounts.token_mint.to_account_info(),
            to: ctx.accounts.destination_token_account.to_account_info(),
            authority: ctx.accounts.bonding_curve.to_account_info(),
        };

        token_2022::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer,
            ),
            transfer_amount,
            9,
        )?;

        // Transfer SOL to destination
        let sol_vault_seeds: &[&[u8]] = &[
            b"sol_vault",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.sol_vault_bump],
        ];

        let transfer_ix = anchor_lang::solana_program::system_instruction::transfer(
            &ctx.accounts.bonding_curve_sol_vault.key(),
            &ctx.accounts.destination.key(),
            bonding_curve.migration_sol,
        );

        anchor_lang::solana_program::program::invoke_signed(
            &transfer_ix,
            &[
                ctx.accounts.bonding_curve_sol_vault.to_account_info(),
                ctx.accounts.destination.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[sol_vault_seeds],
        )?;

        msg!(
            "✅ Withdrawn {} tokens and {} SOL for pool creation",
            transfer_amount,
            bonding_curve.migration_sol
        );

        Ok(())
    }

    /// Update holder count and volume (called by oracle)
    pub fn batch_update_data(
        ctx: Context<BatchUpdateData>,
        holder_count: u64,
        holder_timestamp: i64,
        volume_to_add_cents: u64,
        volume_timestamp: i64,
    ) -> Result<()> {
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        require!(
            bonding_curve.first_buyer_lock_active,
            ErrorCode::NoActiveLock
        );

        // Update holder count
        bonding_curve.current_holder_count = holder_count;
        bonding_curve.last_holder_update = holder_timestamp;

        // Add to cumulative volume
        bonding_curve.total_volume_usd = bonding_curve
            .total_volume_usd
            .checked_add(volume_to_add_cents)
            .ok_or(ErrorCode::MathOverflow)?;
        bonding_curve.last_volume_update = volume_timestamp;

        msg!(
            "📊 Updated: {} holders, ${} total volume",
            holder_count,
            bonding_curve.total_volume_usd / 100
        );

        Ok(())
    }

    /// Check if unlock conditions are met
    pub fn check_unlock_conditions(ctx: Context<CheckUnlockConditions>) -> Result<()> {
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        require!(
            bonding_curve.first_buyer_lock_active,
            ErrorCode::NoActiveLock
        );

        let holders_met = bonding_curve.current_holder_count >= bonding_curve.holder_threshold;
        let volume_met = bonding_curve.total_volume_usd >= bonding_curve.volume_threshold;

        bonding_curve.unlockable = holders_met && volume_met;

        if bonding_curve.unlockable {
            msg!("🎉 Unlock conditions met!");
            msg!(
                "   Holders: {} / {}",
                bonding_curve.current_holder_count,
                bonding_curve.holder_threshold
            );
            msg!(
                "   Volume: ${} / ${}",
                bonding_curve.total_volume_usd / 100,
                bonding_curve.volume_threshold / 100
            );
        } else {
            msg!("⏳ Conditions not yet met");
            msg!(
                "   Holders: {} / {} {}",
                bonding_curve.current_holder_count,
                bonding_curve.holder_threshold,
                if holders_met { "✓" } else { "✗" }
            );
            msg!(
                "   Volume: ${} / ${} {}",
                bonding_curve.total_volume_usd / 100,
                bonding_curve.volume_threshold / 100,
                if volume_met { "✓" } else { "✗" }
            );
        }

        Ok(())
    }

    /// Unlock and release first buyer's locked tokens
    pub fn unlock_first_buyer_tokens(ctx: Context<UnlockFirstBuyerTokens>) -> Result<()> {
        let bonding_curve_info = ctx.accounts.bonding_curve.to_account_info();
        let bonding_curve = &mut ctx.accounts.bonding_curve;

        require!(
            bonding_curve.first_buyer_lock_active,
            ErrorCode::NoActiveLock
        );
        require!(bonding_curve.unlockable, ErrorCode::ConditionsNotMet);
        require!(
            ctx.accounts.first_buyer.key() == bonding_curve.first_buyer.unwrap(),
            ErrorCode::UnauthorizedUnlock
        );

        // Transfer locked tokens to first buyer
        let seeds = &[
            b"bonding_curve",
            bonding_curve.token_mint.as_ref(),
            &[bonding_curve.bump],
        ];
        let signer = &[&seeds[..]];

        let cpi_accounts = token_2022::TransferChecked {
            from: ctx.accounts.first_buyer_lock_vault.to_account_info(),
            mint: ctx.accounts.token_mint.to_account_info(),
            to: ctx.accounts.first_buyer_token_account.to_account_info(),
            authority: bonding_curve_info,
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        token_2022::transfer_checked(cpi_ctx, bonding_curve.first_buyer_locked_amount, 9)?;

        msg!(
            "🔓 Released {} locked tokens to first buyer {}!",
            bonding_curve.first_buyer_locked_amount,
            ctx.accounts.first_buyer.key()
        );

        emit!(FirstBuyerUnlocked {
            token_mint: bonding_curve.token_mint,
            buyer: ctx.accounts.first_buyer.key(),
            amount: bonding_curve.first_buyer_locked_amount,
            timestamp: Clock::get()?.unix_timestamp,
        });

        // Deactivate lock
        bonding_curve.first_buyer_lock_active = false;

        Ok(())
    }
}

// Helper functions
fn calculate_tokens_out(sol_in: u64, sol_reserves: u64, token_reserves: u64) -> Result<u64> {
    let sol_in_u128 = sol_in as u128;
    let sol_reserves_u128 = sol_reserves as u128;
    let token_reserves_u128 = token_reserves as u128;

    let k = sol_reserves_u128
        .checked_mul(token_reserves_u128)
        .ok_or(ErrorCode::MathOverflow)?;

    let new_sol_reserves = sol_reserves_u128
        .checked_add(sol_in_u128)
        .ok_or(ErrorCode::MathOverflow)?;

    let new_token_reserves = k
        .checked_div(new_sol_reserves)
        .ok_or(ErrorCode::MathOverflow)?;

    let tokens_out = token_reserves_u128
        .checked_sub(new_token_reserves)
        .ok_or(ErrorCode::MathOverflow)?;

    Ok(tokens_out as u64)
}

fn calculate_sol_out(tokens_in: u64, token_reserves: u64, sol_reserves: u64) -> Result<u64> {
    let tokens_in_u128 = tokens_in as u128;
    let token_reserves_u128 = token_reserves as u128;
    let sol_reserves_u128 = sol_reserves as u128;

    let k = token_reserves_u128
        .checked_mul(sol_reserves_u128)
        .ok_or(ErrorCode::MathOverflow)?;

    let new_token_reserves = token_reserves_u128
        .checked_add(tokens_in_u128)
        .ok_or(ErrorCode::MathOverflow)?;

    let new_sol_reserves = k
        .checked_div(new_token_reserves)
        .ok_or(ErrorCode::MathOverflow)?;

    let sol_out = sol_reserves_u128
        .checked_sub(new_sol_reserves)
        .ok_or(ErrorCode::MathOverflow)?;

    Ok(sol_out as u64)
}

// Account Structs

#[derive(Accounts)]
pub struct InitializeBondingCurve<'info> {
    #[account(
        init,
        payer = creator,
        space = 8 + BondingCurve::INIT_SPACE,
        seeds = [b"bonding_curve", token_mint.key().as_ref()],
        bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    #[account(
        init,
        payer = creator,
        seeds = [b"token_vault", token_mint.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = bonding_curve,
    )]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        init,
        payer = creator,
        seeds = [b"first_buyer_lock_vault", token_mint.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = bonding_curve,
    )]
    pub first_buyer_lock_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA for holding SOL
    #[account(
        mut,
        seeds = [b"sol_vault", token_mint.key().as_ref()],
        bump
    )]
    pub sol_vault: SystemAccount<'info>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    #[account(mut)]
    pub creator: Signer<'info>,

    /// CHECK: Oracle authority for updating data
    pub oracle_authority: AccountInfo<'info>,  

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(
        mut,
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(mut)]
    pub buyer_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"token_vault", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.token_vault_bump
    )]
    pub bonding_curve_token_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"first_buyer_lock_vault", bonding_curve.token_mint.as_ref()],
        bump
    )]
    pub first_buyer_lock_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA for holding SOL
    #[account(
        mut,
        seeds = [b"sol_vault", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.sol_vault_bump
    )]
    pub bonding_curve_sol_vault: AccountInfo<'info>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MigrateToRaydium<'info> {
    #[account(
        mut,
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    #[account(mut)]
    pub authority: Signer<'info>,

    /// CHECK: Platform authority for receiving fees
    #[account(mut)]
    pub platform_authority: AccountInfo<'info>,

    /// CHECK: PDA for holding SOL
    #[account(
        mut,
        seeds = [b"sol_vault", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.sol_vault_bump
    )]
    pub bonding_curve_sol_vault: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawForPool<'info> {
    #[account(
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    #[account(
        mut,
        seeds = [b"token_vault", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.token_vault_bump
    )]
    pub bonding_curve_token_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA for holding SOL
    #[account(
        mut,
        seeds = [b"sol_vault", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.sol_vault_bump
    )]
    pub bonding_curve_sol_vault: AccountInfo<'info>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: Destination for tokens
    #[account(mut)]
    pub destination: AccountInfo<'info>,

    #[account(mut)]
    pub destination_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BatchUpdateData<'info> {
    #[account(
        mut,
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump,
        has_one = oracle_authority
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    pub oracle_authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct CheckUnlockConditions<'info> {
    #[account(
        mut,
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,
}

#[derive(Accounts)]
pub struct UnlockFirstBuyerTokens<'info> {
    #[account(
        mut,
        seeds = [b"bonding_curve", bonding_curve.token_mint.as_ref()],
        bump = bonding_curve.bump
    )]
    pub bonding_curve: Account<'info, BondingCurve>,

    #[account(
        mut,
        seeds = [b"first_buyer_lock_vault", bonding_curve.token_mint.as_ref()],
        bump
    )]
    pub first_buyer_lock_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub first_buyer_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    #[account(mut)]
    pub first_buyer: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

// Data Structs

#[account]
#[derive(InitSpace)]
pub struct BondingCurve {
    pub token_mint: Pubkey,
    pub creator: Pubkey,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub total_supply: u64,
    pub migration_threshold: u64,
    pub is_migrated: bool,
    pub migration_sol: u64,
    pub migration_tokens: u64,
    // First buyer lock fields
    pub first_buyer: Option<Pubkey>,
    pub first_buyer_locked_amount: u64,
    pub first_buyer_lock_active: bool,

    // Unlock conditions
    pub holder_threshold: u64,
    pub volume_threshold: u64,
    pub current_holder_count: u64,
    pub total_volume_usd: u64,
    pub last_holder_update: i64,
    pub last_volume_update: i64,
    pub unlockable: bool,

    // Oracle authority
    pub oracle_authority: Pubkey,

    pub token_vault_bump: u8,
    pub sol_vault_bump: u8,
    pub bump: u8,
}
// Events
#[event]
pub struct FirstBuyerLocked {
    pub token_mint: Pubkey,
    pub buyer: Pubkey,
    pub amount: u64,
    pub sol_spent: u64,
    pub holder_threshold: u64,
    pub volume_threshold: u64,
    pub timestamp: i64,
}
#[event]
pub struct MigrationThresholdReached {
    pub token_mint: Pubkey,
    pub sol_reserves: u64,
    pub timestamp: i64,
}
#[event]
pub struct MigrationCompleted {
    pub token_mint: Pubkey,
    pub liquidity_sol: u64,
    pub liquidity_tokens: u64,
    pub platform_fee: u64,
    pub timestamp: i64,
}
#[event]
pub struct FirstBuyerUnlocked {
    pub token_mint: Pubkey,
    pub buyer: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}
// Error Codes
#[error_code]
pub enum ErrorCode {
    #[msg("Bonding curve has already migrated to Raydium")]
    AlreadyMigrated,
    #[msg("Slippage tolerance exceeded")]
    SlippageExceeded,
    #[msg("Migration threshold not reached")]
    ThresholdNotReached,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid mint authority")]
    InvalidMintAuthority,
    #[msg("Insufficient liquidity in reserves")]
    InsufficientLiquidity,
    #[msg("Not yet migrated")]
    NotMigrated,
    #[msg("Already withdrawn")]
    AlreadyWithdrawn,
    #[msg("Tokens are locked - cannot sell yet")]
    TokensAreLocked,
    #[msg("No active lock to update")]
    NoActiveLock,
    #[msg("Unlock conditions not met")]
    ConditionsNotMet,
    #[msg("Unauthorized unlock attempt")]
    UnauthorizedUnlock,
}

