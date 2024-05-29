//
// gal_builder.rs: GAL constructor
//
// Given a Blueprint, this module constructs an appropriate GAL
// structure, which can then be written out.
//

use crate::{
    blueprint::{Active, Blueprint, PinMode, OLMC},
    chips::Chip,
    errors::{at_line, Error, ErrorCode, OutputSuffix},
    gal::{self, Bounds, Mode, GAL},
};

pub fn build(blueprint: &Blueprint) -> Result<GAL, Error> {
    let mut gal = GAL::new(blueprint.chip);

    match gal.chip {
        Chip::GAL16V8 | Chip::GAL20V8 => build_galxv8(&mut gal, blueprint)?,
        Chip::GAL22V10 => build_gal22v10(&mut gal, blueprint)?,
        Chip::GAL20RA10 => build_gal20ra10(&mut gal, blueprint)?,
    }

    Ok(gal)
}

////////////////////////////////////////////////////////////////////////
// Chip-specific GAL-building algorithms.
//

fn build_galxv8(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    check_not_gal20ra10(blueprint)?;
    set_sig(gal, blueprint);
    set_mode(gal, blueprint);
    // Are we implementing combinatorial expressions as tristate?
    // Pure combinatorial is only available in simple mode.
    let com_is_tri = gal.get_mode() != Mode::Simple;
    set_tristate(gal, blueprint, com_is_tri);
    set_xors(gal, blueprint);
    set_core_eqns(gal, blueprint)?;
    set_pts(gal);
    Ok(())
}

fn build_gal22v10(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    check_not_gal20ra10(blueprint)?;
    set_sig(gal, blueprint);
    // NB: Needs to be called before the set_eqns, since the set_and
    // logic depends on it.
    //
    // For the 22V10, we always implement combintorial expressions as tristate.
    set_tristate(gal, blueprint, true);
    // Must come before core_eqns, for "needs_flip".
    set_xors(gal, blueprint);
    set_core_eqns(gal, blueprint)?;
    set_arsp_eqns(gal, blueprint)?;
    Ok(())
}

fn build_gal20ra10(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    set_sig(gal, blueprint);
    set_xors(gal, blueprint);
    set_core_eqns(gal, blueprint)?;
    set_aux_eqns(gal, blueprint)?;
    Ok(())
}

////////////////////////////////////////////////////////////////////////
// Functions to set specific components of the GAL.
//

// Write out the signature.
fn set_sig(gal: &mut GAL, blueprint: &Blueprint) {
    // Signature has space for 8 bytes.
    for i in 0..usize::min(blueprint.sig.len(), 8) {
        let c = blueprint.sig[i];
        for j in 0..8 {
            gal.sig[i * 8 + j] = (c << j) & 0x80 != 0;
        }
    }
}

// Build the tristate control bits - set for inputs and tristated outputs.
fn set_tristate(gal: &mut GAL, blueprint: &Blueprint, com_is_tri: bool) {
    // 'com_is_tri' if combinatorial equations are being implemented
    // using fixed-enabled tristate outputs (this necessary on some
    // chips/modes).

    let num_olmcs = blueprint.olmcs.len();
    for (olmc, i) in blueprint.olmcs.iter().zip(0..) {
        let is_tristate = match olmc.output {
            None => olmc.feedback,
            Some((PinMode::Tristate, _)) => true,
            Some((PinMode::Combinatorial, _)) => com_is_tri,
            Some((PinMode::Registered, _)) => false,
        };

        if is_tristate {
            gal.ac1[num_olmcs - 1 - i] = true;
        }
    }
}

// Set the main equation and tristate enable equation.
fn set_core_eqns(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    for (olmc, i) in blueprint.olmcs.iter().zip(0..) {
        let bounds = gal.chip.get_bounds(i);

        match &olmc.output {
            Some((_, term)) => {
                let bounds = adjust_main_bounds(gal, &olmc.output, &bounds);
                gal.add_term(term, &bounds)?;
            }
            None => gal.add_term(&gal::false_term(0), &bounds)?,
        }

        if let Some(term) = &olmc.tri_con {
            at_line(term.line_num, check_tristate(gal.chip, olmc))?;
            gal.add_term(
                term,
                &Bounds {
                    row_offset: 0,
                    max_row: 1,
                    ..bounds
                },
            )?;
        }
    }

    Ok(())
}

// Set the AR and SP equations, unique to the GAL22V10.
fn set_arsp_eqns(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    // AR
    let ar_bounds = Bounds {
        start_row: 0,
        max_row: 1,
        row_offset: 0,
    };
    gal.add_term_opt(&blueprint.ar, &ar_bounds)?;

    // SP
    let sp_bounds = Bounds {
        start_row: 131,
        max_row: 1,
        row_offset: 0,
    };
    gal.add_term_opt(&blueprint.sp, &sp_bounds)?;

    Ok(())
}

// Set ARST, APRST and CLK, only used by GAL20RA10.
fn set_aux_eqns(gal: &mut GAL, blueprint: &Blueprint) -> Result<(), Error> {
    for (olmc, i) in blueprint.olmcs.iter().zip(0..) {
        let bounds = gal.chip.get_bounds(i);

        check_aux(&olmc.clock, olmc, OutputSuffix::CLK)?;
        check_aux(&olmc.arst, olmc, OutputSuffix::ARST)?;
        check_aux(&olmc.aprst, olmc, OutputSuffix::APRST)?;

        if let Some((PinMode::Registered, ref term)) = olmc.output {
            let arst_bounds = Bounds {
                row_offset: 2,
                max_row: 3,
                ..bounds
            };
            gal.add_term_opt(&olmc.arst, &arst_bounds)?;

            let aprst_bounds = Bounds {
                row_offset: 3,
                max_row: 4,
                ..bounds
            };
            gal.add_term_opt(&olmc.aprst, &aprst_bounds)?;

            if olmc.clock.is_none() {
                return at_line(term.line_num, Err(ErrorCode::NoCLK));
            }
        }

        // In non-registered modes we want to set the clock term to its default.
        if olmc.output.is_some() {
            let clock_bounds = Bounds {
                row_offset: 1,
                max_row: 2,
                ..bounds
            };
            gal.add_term_opt(&olmc.clock, &clock_bounds)?;
        }
    }

    Ok(())
}

// Set the XOR bits for inverting outputs, if necessary.
fn set_xors(gal: &mut GAL, blueprint: &Blueprint) {
    let num_olmcs = blueprint.olmcs.len();
    for (olmc, i) in blueprint.olmcs.iter().zip(0..) {
        if olmc.output.is_some() && olmc.active == Active::High {
            gal.xor[num_olmcs - 1 - i] = true;
        }
    }
}

// We don't do anything with the PT bits in the GALxxV8s.
fn set_pts(gal: &mut GAL) {
    for bit in gal.pt.iter_mut() {
        *bit = true;
    }
}

////////////////////////////////////////////////////////////////////////
// Other helper functions.
//

// Adjust the bounds for the main term of there's a tristate enable
// term etc. in the first rows.
fn adjust_main_bounds(gal: &GAL, output: &Option<(PinMode, gal::Term)>, bounds: &Bounds) -> Bounds {
    match gal.chip {
        Chip::GAL16V8 | Chip::GAL20V8 => {
            // Registered outputs don't have a tristate enable, or
            // indeed any pins in simple mode.
            let reg_out = matches!(output, Some((PinMode::Registered, _)));
            if gal.get_mode() == Mode::Simple || reg_out {
                *bounds
            } else {
                // Skip the tristate enable row.
                Bounds {
                    row_offset: 1,
                    ..*bounds
                }
            }
        }
        // Skip tristate enable.
        Chip::GAL22V10 => Bounds {
            row_offset: 1,
            ..*bounds
        },
        // Skip ARST, APRST, CLK.
        Chip::GAL20RA10 => Bounds {
            row_offset: 4,
            ..*bounds
        },
    }
}

// Check that we're not trying to use GAL20RA10-specific features.
fn check_not_gal20ra10(blueprint: &Blueprint) -> Result<(), Error> {
    for olmc in blueprint.olmcs.iter() {
        if let Some(term) = &olmc.clock {
            return at_line(
                term.line_num,
                Err(ErrorCode::DisallowedControl {
                    suffix: OutputSuffix::CLK,
                }),
            );
        }
        if let Some(term) = &olmc.arst {
            return at_line(
                term.line_num,
                Err(ErrorCode::DisallowedControl {
                    suffix: OutputSuffix::ARST,
                }),
            );
        }
        if let Some(term) = &olmc.aprst {
            return at_line(
                term.line_num,
                Err(ErrorCode::DisallowedControl {
                    suffix: OutputSuffix::APRST,
                }),
            );
        }
    }
    Ok(())
}

// Check that the main output is in the right mode to use a tristate.
fn check_tristate(chip: Chip, olmc: &OLMC) -> Result<(), ErrorCode> {
    match olmc.output {
        None => Err(ErrorCode::UndefinedOutput {
            suffix: OutputSuffix::E,
        }),
        Some((PinMode::Registered, _)) if chip == Chip::GAL16V8 || chip == Chip::GAL20V8 => {
            Err(ErrorCode::TristateReg)
        }
        Some((PinMode::Combinatorial, _)) => Err(ErrorCode::UnmatchedTristate),
        _ => Ok(()),
    }
}

fn check_aux(field: &Option<gal::Term>, olmc: &OLMC, suffix: OutputSuffix) -> Result<(), Error> {
    if let Some(ref term) = field {
        at_line(
            term.line_num,
            match olmc.output {
                None => Err(ErrorCode::UndefinedOutput { suffix }),
                Some((PinMode::Registered, _)) => Ok(()),
                _ => Err(ErrorCode::InvalidControl { suffix }),
            },
        )
    } else {
        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////
// GALxV8 analysis - determine which mode to run the chip in.

fn set_mode(gal: &mut GAL, blueprint: &Blueprint) {
    gal.set_mode(analyse_mode(&blueprint.olmcs));
}

fn analyse_mode(olmcs: &[OLMC]) -> Mode {
    assert_eq!(
        olmcs.len(),
        8,
        "analyse_mode must only be called for devices with 8 OLMCs"
    );

    // If there's a registered pin, it's registered mode.
    if olmcs
        .iter()
        .any(|olmc| matches!(olmc.output, Some((PinMode::Registered, _))))
    {
        return Mode::Registered;
    }

    // If there's a tristate, it's complex mode.
    if olmcs
        .iter()
        .any(|olmc| matches!(olmc.output, Some((PinMode::Tristate, _))))
    {
        return Mode::Complex;
    }

    // If we can't use simple mode, use complex mode.
    for (n, olmc) in olmcs.iter().enumerate().filter(|(_, olmc)| olmc.feedback) {
        match olmc.output {
            // Some OLMCs cannot be configured as pure inputs in simple mode.
            None => {
                if n == 3 || n == 4 {
                    return Mode::Complex;
                }
            }
            // OLMC pins cannot be used as combinatorial feedback in simple mode.
            Some(_) => return Mode::Complex,
        }
    }

    // If there is still no mode defined, use simple mode.
    Mode::Simple
}

#[cfg(test)]
mod tests {
    use crate::{blueprint::PinMode, gal::Term};

    use super::*;

    fn olmc(mode: PinMode) -> OLMC {
        OLMC {
            output: Some((
                mode,
                Term {
                    line_num: 0,
                    pins: vec![],
                },
            )),
            active: Active::Low,
            tri_con: None,
            clock: None,
            arst: None,
            aprst: None,
            feedback: false,
        }
    }

    fn olmc_feedback_no_output() -> OLMC {
        OLMC {
            output: None,
            active: Active::Low,
            tri_con: None,
            clock: None,
            arst: None,
            aprst: None,
            feedback: true,
        }
    }

    fn olmc_feedback_and_output() -> OLMC {
        OLMC {
            feedback: true,
            ..olmc(PinMode::Combinatorial)
        }
    }

    #[test]
    fn mode1() {
        let olmcs = [
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Simple);
    }

    #[test]
    fn mode2_tristate_output() {
        let olmcs = [
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Tristate),
            olmc(PinMode::Combinatorial),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Complex);
    }

    #[test]
    fn mode2_olmc3() {
        let olmcs = [
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc_feedback_no_output(),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Complex);
    }

    #[test]
    fn mode2_olmc4() {
        let olmcs = [
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc_feedback_no_output(),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Complex);
    }

    #[test]
    fn mode2_feedback() {
        let olmcs = [
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc(PinMode::Combinatorial),
            olmc_feedback_and_output(),
            olmc(PinMode::Combinatorial),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Complex);
    }

    #[test]
    fn mode3_all_registered() {
        let olmcs = [
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Registered);
    }

    #[test]
    fn mode3_first_tristate() {
        let olmcs = [
            olmc(PinMode::Tristate),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
            olmc(PinMode::Registered),
        ];
        assert_eq!(analyse_mode(&olmcs), Mode::Registered);
    }
}
