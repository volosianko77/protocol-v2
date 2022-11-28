import { BN } from '@project-serum/anchor';
import {
	AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO,
	PRICE_PRECISION,
	PEG_PRECISION,
	ZERO,
	BID_ASK_SPREAD_PRECISION,
	ONE,
	AMM_TO_QUOTE_PRECISION_RATIO,
	QUOTE_PRECISION,
	MARGIN_PRECISION,
	PRICE_DIV_PEG,
	PERCENTAGE_PRECISION,
	BASE_PRECISION,
} from '../constants/numericConstants';
import {
	AMM,
	PositionDirection,
	SwapDirection,
	PerpMarketAccount,
	isVariant,
} from '../types';
import { assert } from '../assert/assert';
import { squareRootBN, clampBN, standardizeBaseAssetAmount } from '..';

import { OraclePriceData } from '../oracles/types';
import {
	calculateRepegCost,
	calculateAdjustKCost,
	calculateBudgetedPeg,
} from './repeg';

import { calculateLiveOracleStd } from './oracles';

export function calculatePegFromTargetPrice(
	targetPrice: BN,
	baseAssetReserve: BN,
	quoteAssetReserve: BN
): BN {
	return BN.max(
		targetPrice
			.mul(baseAssetReserve)
			.div(quoteAssetReserve)
			.add(PRICE_DIV_PEG.div(new BN(2)))
			.div(PRICE_DIV_PEG),
		ONE
	);
}

export function calculateOptimalPegAndBudget(
	amm: AMM,
	oraclePriceData: OraclePriceData
): [BN, BN, BN, boolean] {
	const reservePriceBefore = calculatePrice(
		amm.baseAssetReserve,
		amm.quoteAssetReserve,
		amm.pegMultiplier
	);
	const targetPrice = oraclePriceData.price;
	const newPeg = calculatePegFromTargetPrice(
		targetPrice,
		amm.baseAssetReserve,
		amm.quoteAssetReserve
	);
	const prePegCost = calculateRepegCost(amm, newPeg);

	const totalFeeLB = amm.totalExchangeFee.div(new BN(2));
	const budget = BN.max(ZERO, amm.totalFeeMinusDistributions.sub(totalFeeLB));
	if (budget.lt(prePegCost)) {
		const halfMaxPriceSpread = new BN(amm.maxSpread)
			.div(new BN(2))
			.mul(targetPrice)
			.div(BID_ASK_SPREAD_PRECISION);

		let newTargetPrice: BN;
		let newOptimalPeg: BN;
		let newBudget: BN;
		const targetPriceGap = reservePriceBefore.sub(targetPrice);

		if (targetPriceGap.abs().gt(halfMaxPriceSpread)) {
			const markAdj = targetPriceGap.abs().sub(halfMaxPriceSpread);

			if (targetPriceGap.lt(new BN(0))) {
				newTargetPrice = reservePriceBefore.add(markAdj);
			} else {
				newTargetPrice = reservePriceBefore.sub(markAdj);
			}

			newOptimalPeg = calculatePegFromTargetPrice(
				newTargetPrice,
				amm.baseAssetReserve,
				amm.quoteAssetReserve
			);

			newBudget = calculateRepegCost(amm, newOptimalPeg);
			return [newTargetPrice, newOptimalPeg, newBudget, false];
		}
	}

	return [targetPrice, newPeg, budget, true];
}

export function calculateNewAmm(
	amm: AMM,
	oraclePriceData: OraclePriceData
): [BN, BN, BN, BN] {
	let pKNumer = new BN(1);
	let pKDenom = new BN(1);

	const [targetPrice, _newPeg, budget, checkLowerBound] =
		calculateOptimalPegAndBudget(amm, oraclePriceData);
	let prePegCost = calculateRepegCost(amm, _newPeg);
	let newPeg = _newPeg;

	if (prePegCost.gt(budget) && checkLowerBound) {
		[pKNumer, pKDenom] = [new BN(999), new BN(1000)];
		const deficitMadeup = calculateAdjustKCost(amm, pKNumer, pKDenom);
		assert(deficitMadeup.lte(new BN(0)));
		prePegCost = budget.add(deficitMadeup.abs());
		const newAmm = Object.assign({}, amm);
		newAmm.baseAssetReserve = newAmm.baseAssetReserve.mul(pKNumer).div(pKDenom);
		newAmm.sqrtK = newAmm.sqrtK.mul(pKNumer).div(pKDenom);
		const invariant = newAmm.sqrtK.mul(newAmm.sqrtK);
		newAmm.quoteAssetReserve = invariant.div(newAmm.baseAssetReserve);
		const directionToClose = amm.baseAssetAmountWithAmm.gt(ZERO)
			? PositionDirection.SHORT
			: PositionDirection.LONG;

		const [newQuoteAssetReserve, _newBaseAssetReserve] =
			calculateAmmReservesAfterSwap(
				newAmm,
				'base',
				amm.baseAssetAmountWithAmm.abs(),
				getSwapDirection('base', directionToClose)
			);

		newAmm.terminalQuoteAssetReserve = newQuoteAssetReserve;

		newPeg = calculateBudgetedPeg(newAmm, prePegCost, targetPrice);
		prePegCost = calculateRepegCost(newAmm, newPeg);
	}

	return [prePegCost, pKNumer, pKDenom, newPeg];
}

export function calculateUpdatedAMM(
	amm: AMM,
	oraclePriceData: OraclePriceData
): AMM {
	if (amm.curveUpdateIntensity == 0) {
		return amm;
	}
	const newAmm = Object.assign({}, amm);
	const [prepegCost, pKNumer, pKDenom, newPeg] = calculateNewAmm(
		amm,
		oraclePriceData
	);

	newAmm.baseAssetReserve = newAmm.baseAssetReserve.mul(pKNumer).div(pKDenom);
	newAmm.sqrtK = newAmm.sqrtK.mul(pKNumer).div(pKDenom);
	const invariant = newAmm.sqrtK.mul(newAmm.sqrtK);
	newAmm.quoteAssetReserve = invariant.div(newAmm.baseAssetReserve);
	newAmm.pegMultiplier = newPeg;

	const directionToClose = amm.baseAssetAmountWithAmm.gt(ZERO)
		? PositionDirection.SHORT
		: PositionDirection.LONG;

	const [newQuoteAssetReserve, _newBaseAssetReserve] =
		calculateAmmReservesAfterSwap(
			newAmm,
			'base',
			amm.baseAssetAmountWithAmm.abs(),
			getSwapDirection('base', directionToClose)
		);

	newAmm.terminalQuoteAssetReserve = newQuoteAssetReserve;

	newAmm.totalFeeMinusDistributions =
		newAmm.totalFeeMinusDistributions.sub(prepegCost);
	newAmm.netRevenueSinceLastFunding =
		newAmm.netRevenueSinceLastFunding.sub(prepegCost);

	return newAmm;
}

export function calculateUpdatedAMMSpreadReserves(
	amm: AMM,
	direction: PositionDirection,
	oraclePriceData: OraclePriceData
): { baseAssetReserve: BN; quoteAssetReserve: BN; sqrtK: BN; newPeg: BN } {
	const newAmm = calculateUpdatedAMM(amm, oraclePriceData);
	const [shortReserves, longReserves] = calculateSpreadReserves(
		newAmm,
		oraclePriceData
	);

	const dirReserves = isVariant(direction, 'long')
		? longReserves
		: shortReserves;

	const result = {
		baseAssetReserve: dirReserves.baseAssetReserve,
		quoteAssetReserve: dirReserves.quoteAssetReserve,
		sqrtK: newAmm.sqrtK,
		newPeg: newAmm.pegMultiplier,
	};

	return result;
}

export function calculateBidAskPrice(
	amm: AMM,
	oraclePriceData: OraclePriceData,
	withUpdate = true
): [BN, BN] {
	let newAmm: AMM;
	if (withUpdate) {
		newAmm = calculateUpdatedAMM(amm, oraclePriceData);
	} else {
		newAmm = amm;
	}

	const [bidReserves, askReserves] = calculateSpreadReserves(
		newAmm,
		oraclePriceData
	);

	const askPrice = calculatePrice(
		askReserves.baseAssetReserve,
		askReserves.quoteAssetReserve,
		newAmm.pegMultiplier
	);

	const bidPrice = calculatePrice(
		bidReserves.baseAssetReserve,
		bidReserves.quoteAssetReserve,
		newAmm.pegMultiplier
	);

	return [bidPrice, askPrice];
}

/**
 * Calculates a price given an arbitrary base and quote amount (they must have the same precision)
 *
 * @param baseAssetReserves
 * @param quoteAssetReserves
 * @param pegMultiplier
 * @returns price : Precision PRICE_PRECISION
 */
export function calculatePrice(
	baseAssetReserves: BN,
	quoteAssetReserves: BN,
	pegMultiplier: BN
): BN {
	if (baseAssetReserves.abs().lte(ZERO)) {
		return new BN(0);
	}

	return quoteAssetReserves
		.mul(PRICE_PRECISION)
		.mul(pegMultiplier)
		.div(PEG_PRECISION)
		.div(baseAssetReserves);
}

export type AssetType = 'quote' | 'base';

/**
 * Calculates what the amm reserves would be after swapping a quote or base asset amount.
 *
 * @param amm
 * @param inputAssetType
 * @param swapAmount
 * @param swapDirection
 * @returns quoteAssetReserve and baseAssetReserve after swap. : Precision AMM_RESERVE_PRECISION
 */
export function calculateAmmReservesAfterSwap(
	amm: Pick<
		AMM,
		'pegMultiplier' | 'quoteAssetReserve' | 'sqrtK' | 'baseAssetReserve'
	>,
	inputAssetType: AssetType,
	swapAmount: BN,
	swapDirection: SwapDirection
): [BN, BN] {
	assert(swapAmount.gte(ZERO), 'swapAmount must be greater than 0');

	let newQuoteAssetReserve;
	let newBaseAssetReserve;

	if (inputAssetType === 'quote') {
		swapAmount = swapAmount
			.mul(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO)
			.div(amm.pegMultiplier);

		[newQuoteAssetReserve, newBaseAssetReserve] = calculateSwapOutput(
			amm.quoteAssetReserve,
			swapAmount,
			swapDirection,
			amm.sqrtK.mul(amm.sqrtK)
		);
	} else {
		[newBaseAssetReserve, newQuoteAssetReserve] = calculateSwapOutput(
			amm.baseAssetReserve,
			swapAmount,
			swapDirection,
			amm.sqrtK.mul(amm.sqrtK)
		);
	}

	return [newQuoteAssetReserve, newBaseAssetReserve];
}

export function calculateMarketOpenBidAsk(
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN
): [BN, BN] {
	// open orders
	let openAsks;
	if (maxBaseAssetReserve.gt(baseAssetReserve)) {
		openAsks = maxBaseAssetReserve.sub(baseAssetReserve).mul(new BN(-1));
	} else {
		openAsks = ZERO;
	}

	let openBids;
	if (minBaseAssetReserve.lt(baseAssetReserve)) {
		openBids = baseAssetReserve.sub(minBaseAssetReserve);
	} else {
		openBids = ZERO;
	}
	return [openBids, openAsks];
}

export function calculateInventoryScale(
	baseAssetAmountWithAmm: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	directionalSpread: number,
	maxSpread: number
): number {
	if (baseAssetAmountWithAmm.eq(ZERO)) {
		return 0;
	}

	const defaultLargeBidAskFactor = BID_ASK_SPREAD_PRECISION.mul(new BN(10));
	// inventory skew
	const [openBids, openAsks] = calculateMarketOpenBidAsk(
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve
	);

	const minSideLiquidity = BN.max(
		new BN(1),
		BN.min(openBids.abs(), openAsks.abs())
	);

	const inventoryScaleMax =
		BN.max(
			defaultLargeBidAskFactor,
			new BN(maxSpread / 2)
				.mul(BID_ASK_SPREAD_PRECISION)
				.div(new BN(Math.max(directionalSpread, 1)))
		).toNumber() / BID_ASK_SPREAD_PRECISION.toNumber();

	console.log('inventoryScaleMax:', inventoryScaleMax);
	console.log(
		'openBids/Asks',
		openBids.toNumber(),
		openAsks.toNumber(),
		'minSideLiquidity:',
		minSideLiquidity.toNumber()
	);
	console.log('baseAssetAmountWithAmm', baseAssetAmountWithAmm.toNumber());
	console.log('defaultLargeBidAskFactor', defaultLargeBidAskFactor.toNumber());

	const inventoryScale =
		baseAssetAmountWithAmm
			.mul(BN.max(baseAssetAmountWithAmm.abs(), BASE_PRECISION))
			.div(BASE_PRECISION)
			.mul(defaultLargeBidAskFactor)
			.div(minSideLiquidity)
			.abs()
			.toNumber() / BID_ASK_SPREAD_PRECISION.toNumber();

	console.log('1 + inventoryScale', 1 + inventoryScale);
	const inventorySpreadScale = Math.min(inventoryScaleMax, 1 + inventoryScale);

	return inventorySpreadScale;
}

export function calculateEffectiveLeverage(
	baseSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	netBaseAssetAmount: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN
): number {
	// inventory skew
	const netBaseAssetValue = quoteAssetReserve
		.sub(terminalQuoteAssetReserve)
		.mul(pegMultiplier)
		.div(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO);

	const localBaseAssetValue = netBaseAssetAmount
		.mul(reservePrice)
		.div(AMM_TO_QUOTE_PRECISION_RATIO.mul(PRICE_PRECISION));

	const effectiveLeverage =
		localBaseAssetValue.sub(netBaseAssetValue).toNumber() /
			(Math.max(0, totalFeeMinusDistributions.toNumber()) + 1) +
		1 / QUOTE_PRECISION.toNumber();

	return effectiveLeverage;
}

export function calculateMaxSpread(marginRatioInitial: number): number {
	const maxTargetSpread: number = new BN(marginRatioInitial)
		.mul(BID_ASK_SPREAD_PRECISION.div(MARGIN_PRECISION))
		.toNumber();

	return maxTargetSpread;
}

export function calculateVolSpreadBN(
	lastOracleConfPct: BN,
	reservePrice: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN
): [BN, BN] {
	const marketAvgStdPct = markStd
		.add(oracleStd)
		.mul(PERCENTAGE_PRECISION)
		.div(reservePrice)
		.div(new BN(2));
	const volSpread = BN.max(lastOracleConfPct, marketAvgStdPct.div(new BN(2)));

	const clampMin = PERCENTAGE_PRECISION.div(new BN(100));
	const clampMax = PERCENTAGE_PRECISION.mul(new BN(16)).div(new BN(10));
	console.log(
		'l/s intesnity:',
		volume24H.toString(),
		longIntensity.toNumber(),
		shortIntensity.toNumber()
	);
	console.log('clamps:', clampMin.toNumber(), clampMax.toNumber());
	const longVolSpreadFactor = clampBN(
		longIntensity.mul(PERCENTAGE_PRECISION).div(BN.max(ONE, volume24H)),
		clampMin,
		clampMax
	);
	const shortVolSpreadFactor = clampBN(
		shortIntensity.mul(PERCENTAGE_PRECISION).div(BN.max(ONE, volume24H)),
		clampMin,
		clampMax
	);

	console.log(
		'volspread + l/s factor:',
		volSpread.toString(),
		longVolSpreadFactor.toNumber(),
		shortVolSpreadFactor.toNumber()
	);

	const longVolSpread = BN.max(
		lastOracleConfPct,
		volSpread.mul(longVolSpreadFactor).div(PERCENTAGE_PRECISION)
	);
	const shortVolSpread = BN.max(
		lastOracleConfPct,
		volSpread.mul(shortVolSpreadFactor).div(PERCENTAGE_PRECISION)
	);

	return [longVolSpread, shortVolSpread];
}

export function calculateSpreadBN(
	baseSpread: number,
	lastOracleReservePriceSpreadPct: BN,
	lastOracleConfPct: BN,
	maxSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	baseAssetAmountWithAmm: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN,
	netRevenueSinceLastFunding: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN
): [number, number] {
	const [longVolSpread, shortVolSpread] = calculateVolSpreadBN(
		lastOracleConfPct,
		reservePrice,
		markStd,
		oracleStd,
		longIntensity,
		shortIntensity,
		volume24H
	);
	console.log(
		'vol l/s spreads:',
		longVolSpread.toNumber(),
		shortVolSpread.toNumber()
	);

	let longSpread = Math.max(baseSpread / 2, longVolSpread.toNumber());
	let shortSpread = Math.max(baseSpread / 2, shortVolSpread.toNumber());
	console.log('l/s spread:', longSpread, shortSpread);

	if (lastOracleReservePriceSpreadPct.gt(ZERO)) {
		shortSpread = Math.max(
			shortSpread,
			lastOracleReservePriceSpreadPct.abs().toNumber() +
				shortVolSpread.toNumber()
		);
	} else if (lastOracleReservePriceSpreadPct.lt(ZERO)) {
		longSpread = Math.max(
			longSpread,
			lastOracleReservePriceSpreadPct.abs().toNumber() +
				longVolSpread.toNumber()
		);
	}
	console.log('l/s spread:', longSpread, shortSpread);

	const maxTargetSpread: number = Math.max(
		maxSpread,
		lastOracleReservePriceSpreadPct.abs().toNumber()
	);

	const inventorySpreadScale = calculateInventoryScale(
		baseAssetAmountWithAmm,
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve,
		baseAssetAmountWithAmm.gt(ZERO) ? longSpread : shortSpread,
		maxTargetSpread
	);

	if (baseAssetAmountWithAmm.gt(ZERO)) {
		longSpread *= inventorySpreadScale;
	} else if (baseAssetAmountWithAmm.lt(ZERO)) {
		shortSpread *= inventorySpreadScale;
	}
	console.log('l/s spread:', longSpread, shortSpread);

	const MAX_SPREAD_SCALE = 10;
	if (totalFeeMinusDistributions.gt(ZERO)) {
		const effectiveLeverage = calculateEffectiveLeverage(
			baseSpread,
			quoteAssetReserve,
			terminalQuoteAssetReserve,
			pegMultiplier,
			baseAssetAmountWithAmm,
			reservePrice,
			totalFeeMinusDistributions
		);
		const spreadScale = Math.min(MAX_SPREAD_SCALE, 1 + effectiveLeverage);
		if (baseAssetAmountWithAmm.gt(ZERO)) {
			longSpread *= spreadScale;
		} else {
			shortSpread *= spreadScale;
		}
	} else {
		longSpread *= MAX_SPREAD_SCALE;
		shortSpread *= MAX_SPREAD_SCALE;
	}

	console.log('l/s spread:', longSpread, shortSpread);

	const DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT = new BN(-25).mul(
		QUOTE_PRECISION
	);
	if (
		netRevenueSinceLastFunding.lt(
			DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT
		)
	) {
		const retreatAmount = Math.min(
			maxTargetSpread / 10,
			Math.floor(
				(baseSpread * netRevenueSinceLastFunding.toNumber()) /
					DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT.toNumber()
			)
		);
		const halfRetreatAmount = Math.floor(retreatAmount / 2);
		if (baseAssetAmountWithAmm.gt(ZERO)) {
			longSpread += retreatAmount;
			shortSpread += halfRetreatAmount;
		} else if (baseAssetAmountWithAmm.lt(ZERO)) {
			longSpread += halfRetreatAmount;
			shortSpread += retreatAmount;
		} else {
			longSpread += halfRetreatAmount;
			shortSpread += halfRetreatAmount;
		}
	}
	console.log('l/s spread:', longSpread, shortSpread);

	const totalSpread = longSpread + shortSpread;
	if (totalSpread > maxTargetSpread) {
		if (longSpread > shortSpread) {
			longSpread = Math.ceil((longSpread * maxTargetSpread) / totalSpread);
			shortSpread = maxTargetSpread - longSpread;
		} else {
			shortSpread = Math.ceil((shortSpread * maxTargetSpread) / totalSpread);
			longSpread = maxTargetSpread - shortSpread;
		}
	}
	console.log('l/s spread:', longSpread, shortSpread);

	return [longSpread, shortSpread];
}

export function calculateSpread(
	amm: AMM,
	oraclePriceData: OraclePriceData
): [number, number] {
	if (amm.baseSpread == 0 || amm.curveUpdateIntensity == 0) {
		return [amm.baseSpread / 2, amm.baseSpread / 2];
	}

	const reservePrice = calculatePrice(
		amm.baseAssetReserve,
		amm.quoteAssetReserve,
		amm.pegMultiplier
	);

	const targetPrice = oraclePriceData?.price || reservePrice;
	const confInterval = oraclePriceData.confidence || ZERO;
	const targetMarkSpreadPct = reservePrice
		.sub(targetPrice)
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(reservePrice);

	const confIntervalPct = confInterval
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(reservePrice);

	const now = new BN(new Date().getTime() / 1000); //todo
	const liveOracleStd = calculateLiveOracleStd(amm, oraclePriceData, now);

	const [longSpread, shortSpread] = calculateSpreadBN(
		amm.baseSpread,
		targetMarkSpreadPct,
		confIntervalPct,
		amm.maxSpread,
		amm.quoteAssetReserve,
		amm.terminalQuoteAssetReserve,
		amm.pegMultiplier,
		amm.baseAssetAmountWithAmm,
		reservePrice,
		amm.totalFeeMinusDistributions,
		amm.netRevenueSinceLastFunding,
		amm.baseAssetReserve,
		amm.minBaseAssetReserve,
		amm.maxBaseAssetReserve,
		amm.markStd,
		liveOracleStd,
		amm.longIntensityVolume,
		amm.shortIntensityVolume,
		amm.volume24H
	);

	return [longSpread, shortSpread];
}

export function calculateSpreadReserves(
	amm: AMM,
	oraclePriceData: OraclePriceData
) {
	function calculateSpreadReserve(
		spread: number,
		direction: PositionDirection,
		amm: AMM
	): {
		baseAssetReserve;
		quoteAssetReserve;
	} {
		if (spread === 0) {
			return {
				baseAssetReserve: amm.baseAssetReserve,
				quoteAssetReserve: amm.quoteAssetReserve,
			};
		}

		const quoteAssetReserveDelta = amm.quoteAssetReserve.div(
			BID_ASK_SPREAD_PRECISION.div(new BN(spread / 2))
		);

		let quoteAssetReserve;
		if (isVariant(direction, 'long')) {
			quoteAssetReserve = amm.quoteAssetReserve.add(quoteAssetReserveDelta);
		} else {
			quoteAssetReserve = amm.quoteAssetReserve.sub(quoteAssetReserveDelta);
		}

		const baseAssetReserve = amm.sqrtK.mul(amm.sqrtK).div(quoteAssetReserve);
		return {
			baseAssetReserve,
			quoteAssetReserve,
		};
	}

	const [longSpread, shortSpread] = calculateSpread(amm, oraclePriceData);
	const askReserves = calculateSpreadReserve(
		longSpread,
		PositionDirection.LONG,
		amm
	);
	const bidReserves = calculateSpreadReserve(
		shortSpread,
		PositionDirection.SHORT,
		amm
	);

	return [bidReserves, askReserves];
}

/**
 * Helper function calculating constant product curve output. Agnostic to whether input asset is quote or base
 *
 * @param inputAssetReserve
 * @param swapAmount
 * @param swapDirection
 * @param invariant
 * @returns newInputAssetReserve and newOutputAssetReserve after swap. : Precision AMM_RESERVE_PRECISION
 */
export function calculateSwapOutput(
	inputAssetReserve: BN,
	swapAmount: BN,
	swapDirection: SwapDirection,
	invariant: BN
): [BN, BN] {
	let newInputAssetReserve;
	if (swapDirection === SwapDirection.ADD) {
		newInputAssetReserve = inputAssetReserve.add(swapAmount);
	} else {
		newInputAssetReserve = inputAssetReserve.sub(swapAmount);
	}
	const newOutputAssetReserve = invariant.div(newInputAssetReserve);
	return [newInputAssetReserve, newOutputAssetReserve];
}

/**
 * Translate long/shorting quote/base asset into amm operation
 *
 * @param inputAssetType
 * @param positionDirection
 */
export function getSwapDirection(
	inputAssetType: AssetType,
	positionDirection: PositionDirection
): SwapDirection {
	if (isVariant(positionDirection, 'long') && inputAssetType === 'base') {
		return SwapDirection.REMOVE;
	}

	if (isVariant(positionDirection, 'short') && inputAssetType === 'quote') {
		return SwapDirection.REMOVE;
	}

	return SwapDirection.ADD;
}

/**
 * Helper function calculating terminal price of amm
 *
 * @param market
 * @returns cost : Precision PRICE_PRECISION
 */
export function calculateTerminalPrice(market: PerpMarketAccount) {
	const directionToClose = market.amm.baseAssetAmountWithAmm.gt(ZERO)
		? PositionDirection.SHORT
		: PositionDirection.LONG;

	const [newQuoteAssetReserve, newBaseAssetReserve] =
		calculateAmmReservesAfterSwap(
			market.amm,
			'base',
			market.amm.baseAssetAmountWithAmm.abs(),
			getSwapDirection('base', directionToClose)
		);

	const terminalPrice = newQuoteAssetReserve
		.mul(PRICE_PRECISION)
		.mul(market.amm.pegMultiplier)
		.div(PEG_PRECISION)
		.div(newBaseAssetReserve);

	return terminalPrice;
}

export function calculateMaxBaseAssetAmountToTrade(
	amm: AMM,
	limit_price: BN,
	direction: PositionDirection,
	oraclePriceData?: OraclePriceData
): [BN, PositionDirection] {
	const invariant = amm.sqrtK.mul(amm.sqrtK);

	const newBaseAssetReserveSquared = invariant
		.mul(PRICE_PRECISION)
		.mul(amm.pegMultiplier)
		.div(limit_price)
		.div(PEG_PRECISION);

	const newBaseAssetReserve = squareRootBN(newBaseAssetReserveSquared);
	const [shortSpreadReserves, longSpreadReserves] = calculateSpreadReserves(
		amm,
		oraclePriceData
	);

	const baseAssetReserveBefore: BN = isVariant(direction, 'long')
		? longSpreadReserves.baseAssetReserve
		: shortSpreadReserves.baseAssetReserve;

	if (newBaseAssetReserve.gt(baseAssetReserveBefore)) {
		return [
			newBaseAssetReserve.sub(baseAssetReserveBefore),
			PositionDirection.SHORT,
		];
	} else if (newBaseAssetReserve.lt(baseAssetReserveBefore)) {
		return [
			baseAssetReserveBefore.sub(newBaseAssetReserve),
			PositionDirection.LONG,
		];
	} else {
		console.log('tradeSize Too Small');
		return [new BN(0), PositionDirection.LONG];
	}
}

export function calculateQuoteAssetAmountSwapped(
	quoteAssetReserves: BN,
	pegMultiplier: BN,
	swapDirection: SwapDirection
): BN {
	if (isVariant(swapDirection, 'remove')) {
		quoteAssetReserves = quoteAssetReserves.add(ONE);
	}

	let quoteAssetAmount = quoteAssetReserves
		.mul(pegMultiplier)
		.div(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO);

	if (isVariant(swapDirection, 'remove')) {
		quoteAssetAmount = quoteAssetAmount.add(ONE);
	}

	return quoteAssetAmount;
}

export function calculateMaxBaseAssetAmountFillable(
	amm: AMM,
	orderDirection: PositionDirection
): BN {
	const maxFillSize = amm.baseAssetReserve.div(
		new BN(amm.maxFillReserveFraction)
	);
	let maxBaseAssetAmountOnSide: BN;
	if (isVariant(orderDirection, 'long')) {
		maxBaseAssetAmountOnSide = BN.max(
			ZERO,
			amm.baseAssetReserve.sub(amm.minBaseAssetReserve)
		);
	} else {
		maxBaseAssetAmountOnSide = BN.max(
			ZERO,
			amm.maxBaseAssetReserve.sub(amm.baseAssetReserve)
		);
	}

	return standardizeBaseAssetAmount(
		BN.min(maxFillSize, maxBaseAssetAmountOnSide),
		amm.orderStepSize
	);
}
