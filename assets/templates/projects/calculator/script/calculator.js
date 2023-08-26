import Display from './Display.js';
import Input from './Input.js';

export default class Calculator {
    constructor(className, shadowRoot) {
        this.shadowRoot = shadowRoot;
        this.calculatorElement = this.shadowRoot.querySelector(`.${className}`);
        
        this.initializeComponents();
    }

    initializeComponents() {
        this.display = new Display(this.calculatorElement);
        this.input = new Input(this.calculatorElement);
        this.input.registerDisplayObserver(this.display);
        this.display.reset();
    }
}
